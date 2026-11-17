//! A pooled embedder must still batch.
//!
//! `PooledEmbedder` overrides `embed_batch`, and if it did not, the trait's default would loop
//! `embed` — one HTTP request per text. That is invisible in a passing test suite (the vectors
//! are identical) and only shows up as latency: an add that extracted six memories paid six
//! round trips where an unpooled embedder pays one. These tests count the requests.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Three memories the extractor will accept, so one add writes three rows.
const EXTRACTED: usize = 3;

#[derive(Clone, Default)]
struct Upstream {
    /// The `input` arrays of every embeddings request, in order.
    embeddings: Arc<Mutex<Vec<Vec<String>>>>,
}

async fn chat(State(_up): State<Upstream>, Json(_body): Json<Value>) -> Json<Value> {
    let memories: Vec<Value> = (0..EXTRACTED)
        .map(|i| json!({"id": i.to_string(), "text": format!("memory number {i}")}))
        .collect();
    Json(json!({
        "choices": [{
            "message": {"role": "assistant", "content": json!({"memory": memories}).to_string()},
            "finish_reason": "stop",
            "index": 0,
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }))
}

async fn embeddings(State(up): State<Upstream>, Json(body): Json<Value>) -> Json<Value> {
    let inputs: Vec<String> = body["input"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    up.embeddings.lock().unwrap().push(inputs.clone());
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, _)| json!({"embedding": vec![0.25f32; 8], "index": i}))
        .collect();
    Json(json!({"data": data, "usage": {"total_tokens": 1}}))
}

async fn start() -> (Upstream, String) {
    let up = Upstream::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/embeddings", post(embeddings))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (up, format!("http://{addr}/v1"))
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

/// Two keys, so the embedder really is the pooled implementation.
fn config(base_url: &str) -> Value {
    json!({
        "version": "v1.1",
        "llm": {"provider": "openai", "config": {
            "model": "gpt-4o", "api_key": "sk-a", "openai_base_url": base_url,
        }},
        "embedder": {"provider": "openai", "config": {
            "model": "text-embedding-3-small",
            "api_keys": ["sk-a", "sk-b"],
            "api_key": "sk-a",
            "openai_base_url": base_url,
            "embedding_dims": 8,
        }},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
    })
}

#[tokio::test]
async fn an_add_writes_every_memory_in_one_batched_request() {
    let (up, base) = start().await;
    let app = app();
    let (status, body) = call(&app.router, "POST", "/configure", Some(config(&base))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "three things worth remembering"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["results"].as_array().map(|a| a.len()),
        Some(EXTRACTED),
        "{body}"
    );

    let calls = up.embeddings.lock().unwrap().clone();
    // One request for the existing-memory lookup, one for the three new memories.
    assert_eq!(
        calls.len(),
        2,
        "one request per text means batching is gone: {calls:?}"
    );
    assert_eq!(
        calls[1].len(),
        EXTRACTED,
        "the write should have gone out as a single batch: {calls:?}"
    );
}

#[tokio::test]
async fn a_batch_survives_a_rate_limited_key() {
    // The pool is asked for a key per chunk; a 429 on the first one must be absorbed and the
    // chunk retried on the second, not turned into a failed add.
    let (up, base) = start().await;
    let app = app();
    let (status, body) = call(&app.router, "POST", "/configure", Some(config(&base))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Retire the first key so the next request has to use the second.
    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "first"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = up.embeddings.lock().unwrap().len();

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "second"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = up.embeddings.lock().unwrap().len();
    assert!(
        after > before,
        "the second add should have embedded through the pool"
    );
}

/// The mock above answers 200 to everything; this is the check that it is reachable at all, so a
/// silently broken harness cannot make the assertions above vacuous.
#[tokio::test]
async fn the_harness_receives_requests() {
    let (up, base) = start().await;
    let app = app();
    let (status, _) = call(&app.router, "POST", "/configure", Some(config(&base))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "anything"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!up.embeddings.lock().unwrap().is_empty());
}
