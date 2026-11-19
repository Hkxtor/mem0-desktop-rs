//! The Requests page expands a row: what the operation stored or found, and where its time went.
//!
//! `memory_operations` used to keep only a result *count*, so a row could say "3 results" and
//! nothing about which three. These tests drive a stand-in upstream and read back both the API
//! the dashboard calls and the column it is stored in.

// The pool registry is process-wide and `reset`/`configure` touch it: serialise the tests rather
// than let two of them configure the same global at once.
#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The chat answer this mock returns; replaced per test.
#[derive(Clone, Default)]
struct Upstream {
    answer: Arc<Mutex<String>>,
    /// How long `/chat/completions` waits before answering, so the staged timing has something
    /// measurable to attribute.
    chat_delay_ms: Arc<Mutex<u64>>,
    embed_delay_ms: Arc<Mutex<u64>>,
    decision_delay_ms: Arc<Mutex<u64>>,
}

/// The classification backend the add calls after extraction. It is a separate client from the
/// extracting model, so its time has to show up as `classify_ms` and nowhere else.
async fn decisions(
    State(up): State<Upstream>,
    _headers: HeaderMap,
    _body: Json<Value>,
) -> (StatusCode, String) {
    let delay = *up.decision_delay_ms.lock().unwrap();
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let mut answers = serde_json::Map::new();
    for name in ["technology", "food", "hobbies", "professional_details"] {
        answers.insert(
            name.to_string(),
            json!({"type": "noul", "noul": 0.9, "confidence": 0.9}),
        );
    }
    (
        StatusCode::OK,
        json!({"answers": Value::Object(answers)}).to_string(),
    )
}

async fn chat(State(up): State<Upstream>, _body: Json<Value>) -> (StatusCode, String) {
    let delay = *up.chat_delay_ms.lock().unwrap();
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let content = up.answer.lock().unwrap().clone();
    (
        StatusCode::OK,
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }]
        })
        .to_string(),
    )
}

async fn embed(State(up): State<Upstream>, Json(body): Json<Value>) -> (StatusCode, String) {
    let delay = *up.embed_delay_ms.lock().unwrap();
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    // One vector per input: the client refuses a batched request answered with a single vector.
    let n = body["input"]
        .as_array()
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
        .route("/v1beta/decisions", post(decisions))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (up, format!("http://{addr}"))
}

struct App {
    dir: tempfile::TempDir,
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
    App { dir, router }
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

/// The extraction answer for a set of memory texts, in the shape the model is asked for.
fn answer_of(texts: &[String]) -> String {
    let items: Vec<Value> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": i.to_string(), "text": t}))
        .collect();
    json!({ "memory": items }).to_string()
}

async fn set_answer(up: &Upstream, texts: &[String]) {
    *up.answer.lock().unwrap() = answer_of(texts);
}

async fn add_remembering(app: &Router, text: &str) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": text}],
            "user_id": "u"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The newest recorded operation of `operation`, as `/requests` reports it.
async fn newest(app: &Router, operation: &str) -> Value {
    let (status, body) = call(app, "GET", "/requests", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .expect("a bare array, as before")
        .iter()
        .find(|r| r["operation"] == operation)
        .unwrap_or_else(|| panic!("no {operation} row in {body}"))
        .clone()
}

#[tokio::test]
async fn an_add_row_expands_to_the_entries_it_stored() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    let facts = vec![
        "User moved to Hangzhou in March 2024".to_string(),
        "User rides a bike to work".to_string(),
        "User prefers tea over coffee".to_string(),
    ];
    set_answer(&up, &facts).await;

    let body = add_remembering(&app.router, "I moved to Hangzhou and ride to work").await;
    assert_eq!(body["results"].as_array().unwrap().len(), 3);

    let row = newest(&app.router, "ADD").await;
    let summary = &row["result_summary"];
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["truncated"], false);
    let kept: Vec<&str> = summary["items"]
        .as_array()
        .expect("the add kept its entries")
        .iter()
        .map(|i| i["memory"].as_str().unwrap())
        .collect();
    assert_eq!(kept, facts);
    // The id travels with the text so the expand can jump to the memory.
    assert!(row["result_summary"]["items"][0]["id"].is_string());

    // Not a value the API synthesises: it is a column in `memory_operations`.
    let store = mem0_desktop_rs::store::StateStore::open(&app.dir.path().join("mem0.db")).unwrap();
    let rows = store.list_memory_operations(5).unwrap();
    let stored = rows.iter().find(|r| r["operation"] == "ADD").unwrap();
    assert_eq!(stored["result_summary"], *summary);
}

#[tokio::test]
async fn a_search_row_expands_to_hits_with_their_scores() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    set_answer(&up, &["User prefers tea over coffee".to_string()]).await;
    add_remembering(&app.router, "I like tea").await;

    let (status, body) = call(
        &app.router,
        "POST",
        "/search",
        Some(json!({"query": "tea", "user_id": "u", "limit": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let row = newest(&app.router, "SEARCH").await;
    let items = row["result_summary"]["items"]
        .as_array()
        .expect("a search that hit keeps its hits");
    assert!(!items.is_empty(), "expected a hit: {row}");
    assert_eq!(items[0]["memory"], "User prefers tea over coffee");
    let score = items[0]["score"].as_f64().expect("a hit carries a score");
    // Three decimals, not the float's full tail.
    assert_eq!(score, (score * 1000.0).round() / 1000.0);
}

#[tokio::test]
async fn an_import_row_has_no_detail_to_expand() {
    let _serial = gate();
    let (_up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;

    let (status, body) = call(
        &app.router,
        "POST",
        "/import",
        Some(json!({"memories": [
            {"memory": "imported one", "user_id": "u"},
            {"memory": "imported two", "user_id": "u"},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let row = newest(&app.router, "IMPORT").await;
    assert!(
        row["result_summary"].is_null(),
        "a bulk load states its count in `detail`; it has no per-item detail: {row}"
    );
}

#[tokio::test]
async fn a_long_or_numerous_result_is_abbreviated_and_marked() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    // 25 entries, each far longer than the 80-character budget.
    let facts: Vec<String> = (0..25)
        .map(|i| format!("{} entry {i}", "很长的记忆".repeat(40)))
        .collect();
    set_answer(&up, &facts).await;

    add_remembering(&app.router, "lots of things happened").await;

    let row = newest(&app.router, "ADD").await;
    let summary = &row["result_summary"];
    assert_eq!(summary["total"], 25, "the total is the real count");
    assert_eq!(summary["truncated"], true);
    let items = summary["items"].as_array().unwrap();
    assert_eq!(items.len(), 20, "kept entries are capped");
    let kept = items[0]["memory"].as_str().unwrap();
    assert!(kept.ends_with('…'), "a cut entry says so: {kept}");
    assert_eq!(
        kept.chars().count(),
        81,
        "80 characters plus the ellipsis, cut by characters not bytes"
    );
}

#[tokio::test]
async fn a_failed_add_keeps_its_explanation_and_has_no_detail_to_expand() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    // An answer with nothing in it: the add stored nothing, which `reason` explains.
    *up.answer.lock().unwrap() = r#"{"memory": []}"#.to_string();

    let _ = add_remembering(&app.router, "nothing worth remembering").await;

    let row = newest(&app.router, "ADD").await;
    assert!(row["result_summary"].is_null(), "{row}");
    assert!(
        row["reason"].is_string(),
        "the row still explains itself: {row}"
    );
}

#[tokio::test]
async fn the_row_reports_where_the_time_went() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    set_answer(&up, &["User prefers tea over coffee".to_string()]).await;
    *up.chat_delay_ms.lock().unwrap() = 300;
    *up.embed_delay_ms.lock().unwrap() = 100;

    add_remembering(&app.router, "I like tea").await;

    let row = newest(&app.router, "ADD").await;
    let stages = row["stages"]
        .as_object()
        .unwrap_or_else(|| panic!("an add runs inside a timed scope: {row}"));
    let shown = row["stages"].clone();
    let llm = stages["llm_ms"].as_f64().expect("llm_ms");
    let embed = stages["embed_ms"].as_f64().expect("embed_ms");
    let classify = stages["classify_ms"].as_f64().expect("classify_ms");
    let other = stages["other_ms"].as_f64().expect("other_ms");
    // The mock held the model call for 300ms and every embedding round for 100ms, so the split
    // has to show up rather than being lumped into the total.
    assert!(llm >= 280.0, "the model call is the slow step: {shown}");
    assert!(embed >= 90.0, "the embedding call is timed too: {shown}");
    assert!(other >= 0.0, "stages are clamped, never negative: {shown}");
    let total = row["latency_ms"].as_f64().unwrap();
    // Every stage is part of the same wall clock, so they can never add up to more than it.
    assert!(
        llm + embed + classify + other <= total + 1.0,
        "stages {shown} overrun the {total}ms total"
    );
}

#[tokio::test]
async fn classification_time_is_its_own_stage() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    // A classifier is configured, so the add runs one classification per extracted memory after
    // the extraction. The decision client is not the `Llm` the extractor uses, which is exactly
    // why this has to be measured separately instead of falling into `llm_ms`.
    let cfg = json!({
        "version": "v1.1",
        "llm": {"provider": "openai", "config": {
            "model": "gpt-4o", "openai_base_url": &base, "api_key": "k"
        }},
        "embedder": {"provider": "openai", "config": {
            "model": "text-embedding-3-small", "openai_base_url": &base,
            "api_key": "k", "embedding_dims": 10
        }},
        "vector_store": {"provider": "sqlite-vec", "config": {
            "collection_name": "mem0", "embedding_model_dims": 10
        }},
        "systemone": {"provider": "systemone", "config": {
            "api_key": "k", "base_url": format!("{base}/v1beta/decisions"),
            "model": "gemini-decision-1"
        }},
    });
    let (status, body) = call(&app.router, "POST", "/configure", Some(cfg)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    set_answer(
        &up,
        &[
            "User prefers tea".to_string(),
            "User rides a bike".to_string(),
        ],
    )
    .await;
    *up.decision_delay_ms.lock().unwrap() = 200;

    add_remembering(&app.router, "I like tea and cycling").await;

    let row = newest(&app.router, "ADD").await;
    let stages = row["stages"].clone();
    let classify = stages["classify_ms"].as_f64().unwrap_or(0.0);
    let llm = stages["llm_ms"].as_f64().unwrap_or(0.0);
    assert!(
        classify >= 180.0,
        "the stalled classifier is the slow step: {stages}"
    );
    assert!(
        llm < 150.0,
        "classification is a separate stage, not part of the model extraction: {stages}"
    );
}

#[tokio::test]
async fn two_adds_in_flight_at_once_keep_their_own_stages() {
    let _serial = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    set_answer(&up, &["User prefers tea over coffee".to_string()]).await;
    // Both adds wait on the same upstream for the same time. A shared counter would report the
    // sum of the two waits on each row; a per-request one reports each wait once.
    *up.chat_delay_ms.lock().unwrap() = 200;

    let first = tokio::spawn({
        let router = app.router.clone();
        async move { add_remembering(&router, "I like tea").await }
    });
    let second = tokio::spawn({
        let router = app.router.clone();
        async move { add_remembering(&router, "I ride a bike").await }
    });
    let (a, b) = (first.await.unwrap(), second.await.unwrap());
    assert_eq!(a["results"].as_array().unwrap().len(), 1, "{a}");
    assert_eq!(b["results"].as_array().unwrap().len(), 1, "{b}");

    let (status, body) = call(&app.router, "GET", "/requests", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let adds: Vec<Value> = body
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["operation"] == "ADD" && !r["stages"].is_null())
        .cloned()
        .collect();
    assert_eq!(adds.len(), 2, "both adds were measured: {body}");
    for row in &adds {
        let llm = row["stages"]["llm_ms"].as_f64().unwrap();
        assert!(llm >= 180.0, "each add waited on the model: {row}");
        assert!(
            llm < 350.0,
            "an add must report its own wait, not the other add's as well: {row}"
        );
    }
}
