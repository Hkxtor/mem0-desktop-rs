//! `error_kind`: the Requests page saying *what kind* of upstream failure it was.
//!
//! `reason` already says which step produced nothing — "the model call failed" — which is not
//! enough to act on: a quota passes, a rejected key never does, a wrong model name is a typo to
//! fix, and an outage is not the operator's problem at all. The kind comes from the upstream
//! HTTP status, so these tests drive a mock that can answer each one.
//!
//! The negative case matters just as much as the positive ones: an add the model simply had
//! nothing to say about must not be labelled as a failure. A wrong kind sends the operator to
//! fix something that is not broken.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The key pool registry is process-wide, so tests that configure it must not interleave.
fn gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// An upstream whose chat status the test chooses.
#[derive(Clone)]
struct Upstream {
    /// `200` answers normally; anything else is returned as a failure with that status.
    chat_status: Arc<AtomicU16>,
    /// The extraction answer when the chat call succeeds. `{"memory": []}` is a model that had
    /// nothing to remember — a legitimate answer, not a failure.
    answer: Arc<Mutex<String>>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            chat_status: Arc::new(AtomicU16::new(200)),
            answer: Arc::new(Mutex::new(
                json!({"memory": [{"id": "0", "text": "I write Rust"}]}).to_string(),
            )),
        }
    }
}

async fn chat(State(up): State<Upstream>, _body: Json<Value>) -> (StatusCode, String) {
    let status = up.chat_status.load(Ordering::SeqCst);
    if status != 200 {
        let code = StatusCode::from_u16(status).unwrap();
        return (
            code,
            json!({"error": {"message": format!("provider said {status}")}}).to_string(),
        );
    }
    let answer = up.answer.lock().unwrap().clone();
    (
        StatusCode::OK,
        json!({"choices": [{"message": {"role": "assistant", "content": answer}}]}).to_string(),
    )
}

async fn embed(Json(body): Json<Value>) -> (StatusCode, String) {
    // One vector per input: the client batches, and a short answer is a decode error.
    let n = body
        .get("input")
        .and_then(|i| i.as_array())
        .map(|a| a.len())
        .unwrap_or(1)
        .max(1);
    let data: Vec<Value> = (0..n)
        .map(|i| json!({"embedding": vec![0.1_f32; 10], "index": i}))
        .collect();
    (StatusCode::OK, json!({"data": data}).to_string())
}

async fn start() -> (Upstream, String) {
    let up = Upstream::default();
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

async fn add(app: &Router, content: &str) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": content}],
            "user_id": "u",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The most recent ADD row from the Requests endpoint.
async fn last_add(app: &Router) -> Value {
    let (status, rows) = call(app, "GET", "/requests?limit=10", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    rows.as_array()
        .and_then(|rows| rows.iter().find(|r| r["operation"] == "ADD").cloned())
        .unwrap_or(Value::Null)
}

// The guard is held across the awaits on purpose: it serialises the tests, it is not state a
// task could block on, and dropping it early would let the next test reconfigure the pool
// mid-add.
#[allow(clippy::await_holding_lock)]
async fn kind_after(status: u16) -> Value {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    up.chat_status.store(status, Ordering::SeqCst);
    add(&app.router, "I moved to Hangzhou and ride to work").await;
    last_add(&app.router).await
}

#[tokio::test]
async fn a_429_is_reported_as_a_quota_problem() {
    let row = kind_after(429).await;
    assert_eq!(row["reason"], json!("provider_error"), "{row}");
    assert_eq!(row["error_kind"], json!("rate_limited"), "{row}");
}

#[tokio::test]
async fn a_rejected_key_is_reported_as_an_auth_problem() {
    let row = kind_after(401).await;
    assert_eq!(row["error_kind"], json!("auth_failed"), "{row}");

    let row = kind_after(403).await;
    assert_eq!(row["error_kind"], json!("auth_failed"), "{row}");
}

#[tokio::test]
async fn a_missing_model_is_reported_as_a_not_found_problem() {
    let row = kind_after(404).await;
    assert_eq!(row["error_kind"], json!("model_not_found"), "{row}");
}

#[tokio::test]
async fn an_outage_is_reported_as_an_upstream_problem() {
    let row = kind_after(500).await;
    assert_eq!(row["error_kind"], json!("upstream_error"), "{row}");

    let row = kind_after(503).await;
    assert_eq!(row["error_kind"], json!("upstream_error"), "{row}");
}

/// The most important case: a provider that answered fine and had nothing to remember is not a
/// failure, so it gets no kind. Labelling it would tell the operator to fix a working key.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // the guard is the test serialiser, see `kind_after`
async fn a_model_with_nothing_to_say_has_no_kind() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    *up.answer.lock().unwrap() = json!({"memory": []}).to_string();
    add(&app.router, "hi").await;
    let row = last_add(&app.router).await;
    assert_eq!(row["reason"], json!("no_memories"), "{row}");
    assert_eq!(
        row["error_kind"],
        Value::Null,
        "an empty answer is not a failure: {row}"
    );
}

/// A stored row carries no kind either — the column describes failures only.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // the guard is the test serialiser, see `kind_after`
async fn a_successful_add_has_no_kind() {
    let _g = gate();
    let (_up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    let body = add(&app.router, "I write Rust").await;
    assert_eq!(
        body["results"].as_array().map(|a| a.len()),
        Some(1),
        "{body}"
    );
    let row = last_add(&app.router).await;
    assert_eq!(row["error_kind"], Value::Null, "{row}");
    assert_eq!(row["reason"], Value::Null, "{row}");
}

/// The kind travels with the failure kind it explains, not instead of it: the Requests page
/// shows both "what step failed" and "what to do about it".
#[tokio::test]
async fn the_kind_sits_beside_the_reason_and_the_stash() {
    let row = kind_after(429).await;
    assert_eq!(row["reason"], json!("provider_error"), "{row}");
    assert_eq!(row["error_kind"], json!("rate_limited"), "{row}");
    assert!(
        row["failed_add_id"].is_number(),
        "the input is still kept for a retry: {row}"
    );
}
