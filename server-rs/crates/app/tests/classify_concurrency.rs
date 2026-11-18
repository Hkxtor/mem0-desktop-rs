//! Auto-classification after an add must not be serial.
//!
//! `Memory::add` already batches the embeddings and the insert; classifying each extracted memory
//! one at a time gave all of that back. These tests measure the wall clock with a classifier that
//! deliberately stalls, so a serial implementation cannot pass by accident.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// How many memories one add extracts.
const EXTRACTED: usize = 6;
/// Each classification takes this long. Serial, six of them cost 6×; concurrent, about 2×.
const CLASSIFY_STALL: Duration = Duration::from_millis(400);

async fn llm(Json(_body): Json<Value>) -> Json<Value> {
    let memories: Vec<Value> = (0..EXTRACTED)
        .map(|i| json!({"id": i.to_string(), "text": format!("fact number {i}")}))
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

async fn embed(Json(body): Json<Value>) -> Json<Value> {
    let n = body["input"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(1)
        .max(1);
    let data: Vec<Value> = (0..n)
        .map(|i| json!({"embedding": vec![0.1f32; 8], "index": i}))
        .collect();
    Json(json!({"data": data}))
}

/// The Gemini decision protocol: one request per criterion, stalled on purpose.
async fn decisions(_headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    tokio::time::sleep(CLASSIFY_STALL).await;
    // `answers` is a map keyed by category name, not an array — one entry per question asked.
    let mut answers = serde_json::Map::new();
    for name in ["technology", "food", "hobbies", "professional_details"] {
        answers.insert(
            name.to_string(),
            json!({"type": "noul", "noul": 0.9, "confidence": 0.9}),
        );
    }
    let _ = body;
    Json(json!({"answers": Value::Object(answers)}))
}

async fn start() -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(llm))
        .route("/v1/embeddings", post(embed))
        .route("/v1beta/decisions", post(decisions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
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

#[tokio::test]
async fn an_add_classifies_its_memories_concurrently() {
    let base = start().await;
    let app = app();
    let (status, body) = call(
        &app.router,
        "POST",
        "/configure",
        Some(json!({
            "llm": {"provider": "openai", "config": {
                "model": "gpt-4o", "api_key": "sk-a", "openai_base_url": format!("{base}/v1"),
            }},
            "embedder": {"provider": "openai", "config": {
                "model": "text-embedding-3-small", "api_key": "sk-a",
                "openai_base_url": format!("{base}/v1"), "embedding_dims": 8,
            }},
            "vector_store": {"provider": "sqlite-vec",
                             "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
            "systemone": {"provider": "systemone", "config": {
                "api_key": "sk-a",
                "base_url": format!("{base}/v1beta/decisions"),
                "model": "gemini-decision-1",
            }},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let started = Instant::now();
    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "six facts"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    let wall = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), EXTRACTED, "{body}");
    // The categories still landed, and on the right rows.
    for r in results {
        assert!(
            !r["categories"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true),
            "every memory should be tagged: {body}"
        );
    }

    // Serial would be 6 × 400ms of pure stall = 2400ms, plus the extraction and the embeddings.
    // With a concurrency of four it is two waves = 800ms. The ceiling is deliberately loose so a
    // busy CI box does not turn a speed assertion into a flake — the stall is generous for the
    // same reason — but tight enough that serial cannot sneak under it.
    let serial = CLASSIFY_STALL * EXTRACTED as u32;
    assert!(
        wall < serial.mul_f32(0.6),
        "wall {wall:?} is not better than serial {serial:?}: classification is still one at a time"
    );
    println!("wall={wall:?} for {EXTRACTED} classifications, serial would be {serial:?}");
}

/// The response order must survive concurrency: two identical texts are two different rows.
#[tokio::test]
async fn categories_land_on_the_row_they_belong_to() {
    let base = start().await;
    let app = app();
    let (status, _) = call(
        &app.router,
        "POST",
        "/configure",
        Some(json!({
            "llm": {"provider": "openai", "config": {
                "model": "gpt-4o", "api_key": "sk-a", "openai_base_url": format!("{base}/v1"),
            }},
            "embedder": {"provider": "openai", "config": {
                "model": "text-embedding-3-small", "api_key": "sk-a",
                "openai_base_url": format!("{base}/v1"), "embedding_dims": 8,
            }},
            "vector_store": {"provider": "sqlite-vec",
                             "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
            "systemone": {"provider": "systemone", "config": {
                "api_key": "sk-a",
                "base_url": format!("{base}/v1beta/decisions"),
                "model": "gemini-decision-1",
            }},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "six facts"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results = body["results"].as_array().unwrap();
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["memory"], json!(format!("fact number {i}")), "{body}");
    }
}
