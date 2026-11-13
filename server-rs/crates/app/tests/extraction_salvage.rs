//! A cut-off extraction answer keeps what it finished writing.
//!
//! Field symptom: an ADD returned 200 with `results: []` and the Requests row said
//! "回答无法解析", while the log showed the model had clearly written memories. The cause is
//! the model running out of output budget part-way through the `{"memory": [...]}` array:
//! everything before the cut is valid, the tail is not, and the whole answer used to be
//! thrown away. These tests drive a mock upstream that answers with an exactly-truncated
//! document, which is the one thing a real provider cannot be asked to do on demand.

// Holding a std mutex across await is wrong in production and exactly right here: the guard's
// only job is to stop two tests from configuring the process-wide pool at the same time.
#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The chat answer this mock returns; changed per test through `Upstream::answer`.
#[derive(Clone, Default)]
struct Upstream {
    answer: Arc<Mutex<String>>,
}

async fn chat(State(up): State<Upstream>, _body: Json<Value>) -> (StatusCode, String) {
    let content = up.answer.lock().unwrap().clone();
    (
        StatusCode::OK,
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": content},
                "finish_reason": "length"
            }]
        })
        .to_string(),
    )
}

async fn embed(_body: Json<Value>) -> (StatusCode, String) {
    (
        StatusCode::OK,
        // `index` is required by the client's decoder: a response without it fails to parse.
        json!({"data": [{"embedding": vec![0.1_f32; 10], "index": 0}]}).to_string(),
    )
}

async fn start(answer: &str) -> (Upstream, String) {
    let up = Upstream {
        answer: Arc::new(Mutex::new(answer.to_string())),
    };
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/embeddings", post(embed))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (up, format!("http://{addr}"))
}

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

/// The pool registry is process-wide, so tests that configure the runtime must not interleave.
fn gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    mem0_desktop_rs::pooled::reset();
    let dir = tempfile::tempdir().unwrap();
    let router = mem0_desktop_rs::app_for_test(dir.path().to_path_buf());
    App { _dir: dir, router }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn configure(app: &Router, base_url: &str) {
    let cfg = json!({
        "version": "v1.1",
        "llm": {"provider": "openai", "config": {
            "model": "gpt-4o", "openai_base_url": base_url, "api_key": "k"
        }},
        "embedder": {"provider": "openai", "config": {
            "model": "text-embedding-3-small", "openai_base_url": base_url,
            "api_key": "k", "embedding_dims": 10
        }},
        "vector_store": {"provider": "sqlite-vec", "config": {
            "collection_name": "mem0", "embedding_model_dims": 10
        }},
    });
    let (status, body) = call(app, "POST", "/configure", Some(cfg)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn add(app: &Router) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I moved to Hangzhou and ride to work"}],
            "user_id": "u"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The app logger is process-wide (`logging::init` is a one-shot), so every test in this
/// binary writes to one file. Tests are serialised by `gate`, and each one identifies its own
/// line by the answer content rather than by recency.
/// Call before the add under test: the logger drops lines written before `init`.
fn log_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("mem0-salvage-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("log dir");
        mem0_desktop_rs::logging::init(&dir);
        dir
    })
}

/// The extraction line for one particular model answer, if that answer was logged at all.
fn extraction_line(answer_fragment: &str) -> Option<Value> {
    let path = log_dir().join("logs").join("app.log");
    let raw = std::fs::read_to_string(path).ok()?;
    raw.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .rfind(|l| {
            l["msg"]
                .as_str()
                .is_some_and(|m| m.starts_with("extraction"))
                && l["fields"]["answer"]
                    .as_str()
                    .is_some_and(|a| a.contains(answer_fragment))
        })
}

/// The lost case: two finished memories and a budget cut in the third.
const TRUNCATED: &str = concat!(
    r#"{"memory": [{"id": "0", "text": "User moved to Hangzhou in March 2024"},"#,
    r#" {"id": "1", "text": "User rides 15 minutes to the office"},"#,
    r#" {"id": "2", "text": "User's daughter Mia tur"#
);

#[tokio::test]
async fn a_cut_off_answer_still_stores_the_memories_it_finished() {
    let _gate = gate();
    log_dir();
    let (_up, base) = start(TRUNCATED).await;
    let a = app();
    configure(&a.router, &base).await;

    let added = add(&a.router).await;
    let stored: Vec<String> = added["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results: {added}"))
        .iter()
        .filter_map(|r| r["memory"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        stored,
        vec![
            "User moved to Hangzhou in March 2024",
            "User rides 15 minutes to the office"
        ],
        "the two complete entries survive; the cut one cannot: {added}"
    );

    // And the repair is not silent: the log says the answer was incomplete.
    let line = extraction_line("Mia tur").expect("a log line for the extraction");
    assert_eq!(line["fields"]["stage"], json!("salvaged"), "{line}");
    assert_eq!(line["fields"]["parsed"], json!(2), "{line}");
    assert!(
        line["msg"]
            .as_str()
            .unwrap()
            .contains("kept the complete entries"),
        "{line}"
    );

    // The Requests row is a normal success — something was stored.
    let (_, ops) = call(&a.router, "GET", "/requests?limit=1", None).await;
    assert_eq!(ops[0]["results"], json!(2), "{ops}");
    assert_eq!(ops[0]["reason"], Value::Null, "{ops}");
}

/// Nothing recoverable: unchanged behaviour, still a total loss the page explains.
#[tokio::test]
async fn an_answer_with_no_complete_entry_is_still_unparsed() {
    let _gate = gate();
    log_dir();
    let (_up, base) = start(r#"{"memory": [{"id": "0", "text": "cut befo"#).await;
    let a = app();
    configure(&a.router, &base).await;

    let added = add(&a.router).await;
    assert_eq!(added["results"], json!([]), "{added}");

    let line = extraction_line("cut befo").expect("a log line");
    assert_eq!(line["fields"]["stage"], json!("unparsed"), "{line}");
    let err = line["fields"]["json_error"].as_str().unwrap_or_default();
    assert!(err.contains("EOF while parsing"), "{line}");
    assert!(
        line["fields"]["answer_tail"]
            .as_str()
            .is_some_and(|t| t.contains("cut befo")),
        "the tail shows where it stopped: {line}"
    );

    let (_, ops) = call(&a.router, "GET", "/requests?limit=1", None).await;
    assert_eq!(ops[0]["reason"], json!("unparsed_answer"), "{ops}");
}

/// A whole answer takes the ordinary path: no salvage, no warning about the answer.
#[tokio::test]
async fn a_complete_answer_is_untouched() {
    let _gate = gate();
    log_dir();
    let (_up, base) = start(r#"{"memory": [{"id": "0", "text": "User moved to Hangzhou"}]}"#).await;
    let a = app();
    configure(&a.router, &base).await;

    let added = add(&a.router).await;
    assert_eq!(
        added["results"][0]["memory"], "User moved to Hangzhou",
        "{added}"
    );
    assert!(
        extraction_line("User moved to Hangzhou\"}]}").is_none(),
        "a clean extraction logs nothing about itself"
    );
}
