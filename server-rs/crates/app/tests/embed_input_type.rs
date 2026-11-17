//! `embedder.config.input_type`: the asymmetric-model handshake.
//!
//! NVIDIA's nemotron-3.5 embedders refuse a request without `input_type`, while OpenAI rejects
//! a request that carries it. So the field is opt-in: absent by default, `auto` derives it from
//! the call (storing = passage, searching = query), and a fixed value pins it. These tests read
//! the bodies the embedder actually sent to a stand-in upstream.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Mock {
    bodies: Arc<Mutex<Vec<Value>>>,
}

async fn embeddings(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    let inputs = body["input"].as_array().cloned().unwrap_or_default();
    mock.bodies.lock().unwrap().push(body);
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, _)| json!({"embedding": vec![0.5f32; 8], "index": i}))
        .collect();
    Json(json!({ "data": data, "usage": {"total_tokens": 1} }))
}

async fn start() -> (Mock, String) {
    let mock = Mock::default();
    let app = Router::new()
        .route("/v1/embeddings", post(embeddings))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (mock, format!("http://{addr}/v1"))
}

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
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

fn config(base_url: &str, input_type: Option<&str>) -> Value {
    let mut embedder = json!({
        "model": "nvidia/nemotron-3.5-embed-8b",
        "api_key": "k",
        "openai_base_url": base_url,
        "embedding_dims": 8,
    });
    if let Some(v) = input_type {
        embedder["input_type"] = json!(v);
    }
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "openai", "config": embedder},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
    })
}

async fn configure(app: &Router, cfg: Value) -> (StatusCode, Value) {
    call(app, "POST", "/configure", Some(cfg)).await
}

async fn add_raw(app: &Router, text: &str) -> StatusCode {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": text}],
            "user_id": "u",
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    status
}

async fn search(app: &Router, query: &str) -> StatusCode {
    let (status, body) = call(
        app,
        "POST",
        "/search",
        Some(json!({"query": query, "user_id": "u", "limit": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    status
}

/// The `input_type` of every request the upstream saw, in order.
fn seen(mock: &Mock) -> Vec<Option<String>> {
    mock.bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| {
            b.get("input_type")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn absent_by_default_so_openai_never_sees_the_field() {
    let (mock, base) = start().await;
    let app = app();
    let (status, body) = configure(&app.router, config(&base, None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    add_raw(&app.router, "I keep a sourdough starter").await;
    search(&app.router, "bread").await;

    let seen = seen(&mock);
    assert!(!seen.is_empty(), "no embedding requests reached the mock");
    assert!(
        seen.iter().all(Option::is_none),
        "the field must be left out entirely: {seen:?}"
    );
}

#[tokio::test]
async fn auto_sends_passage_for_storage_and_query_for_search() {
    let (mock, base) = start().await;
    let app = app();
    let (status, body) = configure(&app.router, config(&base, Some("auto"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    add_raw(&app.router, "I keep a sourdough starter").await;
    search(&app.router, "bread").await;

    let seen = seen(&mock);
    assert_eq!(seen.len(), 2, "expected one embedding per call: {seen:?}");
    assert_eq!(seen[0].as_deref(), Some("passage"), "storing is a passage");
    assert_eq!(seen[1].as_deref(), Some("query"), "searching is a query");
}

#[tokio::test]
async fn a_fixed_value_pins_every_call() {
    let (mock, base) = start().await;
    let app = app();
    let (status, body) = configure(&app.router, config(&base, Some("passage"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    add_raw(&app.router, "one").await;
    search(&app.router, "two").await;

    let seen = seen(&mock);
    assert_eq!(seen, vec![Some("passage".into()), Some("passage".into())]);
}

#[tokio::test]
async fn clearing_the_field_removes_it_instead_of_leaving_it_behind() {
    let (mock, base) = start().await;
    let app = app();
    assert_eq!(
        configure(&app.router, config(&base, Some("auto"))).await.0,
        StatusCode::OK
    );
    let stored = call(&app.router, "GET", "/configure", None).await.1;
    assert_eq!(stored["embedder"]["config"]["input_type"], "auto");

    // What the dashboard sends when the select goes back to "do not send": an empty string,
    // because an omitted key means "keep what is stored" in the merge.
    assert_eq!(
        configure(&app.router, config(&base, Some(""))).await.0,
        StatusCode::OK
    );
    let stored = call(&app.router, "GET", "/configure", None).await.1;
    assert!(
        stored["embedder"]["config"].get("input_type").is_none(),
        "an empty value must not survive in the stored config: {stored}"
    );

    add_raw(&app.router, "I keep a sourdough starter").await;
    let seen = seen(&mock);
    assert!(
        seen.iter().all(Option::is_none),
        "the field came back after being cleared: {seen:?}"
    );
}

#[tokio::test]
async fn a_typo_is_rejected_instead_of_silently_sending_nothing() {
    let (_mock, base) = start().await;
    let app = app();
    let (status, body) = configure(&app.router, config(&base, Some("sideways"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let detail = body.to_string();
    assert!(
        detail.contains("not one of: auto, query, passage"),
        "the rejection must say what is allowed: {detail}"
    );
}
