//! The background classification queue, from the outside.
//!
//! Classification moved out of the add so the add stops paying for it. That trade has its own
//! failure modes, and they are what this file pins: a broken classifier must not stop the queue
//! or the add, an unconfigured app must not accumulate work, and nothing is retried in a loop.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The upstream the classifier talks to, and how many times it was asked.
///
/// Every add in this file is `infer: false`, so the model is never called for extraction — the
/// only caller is the classifier. That makes one route enough: answer categories, or fail, and
/// count the attempts.
#[derive(Default)]
struct Upstream {
    attempts: AtomicUsize,
    broken: bool,
}

async fn chat(State(up): State<Arc<Upstream>>) -> (StatusCode, Json<Value>) {
    up.attempts.fetch_add(1, Ordering::SeqCst);
    if up.broken {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": "upstream down"}})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "[\"food\"]"},
                "finish_reason": "stop",
                "index": 0,
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })),
    )
}

async fn embed(Json(body): Json<Value>) -> Json<Value> {
    let n = body["input"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(1)
        .max(1);
    let data: Vec<Value> = (0..n)
        .map(|i| json!({"embedding": vec![0.1f32; 10], "index": i}))
        .collect();
    Json(json!({"data": data}))
}

async fn start(broken: bool) -> (String, Arc<Upstream>) {
    let up = Arc::new(Upstream {
        broken,
        ..Default::default()
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/embeddings", post(embed))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), up)
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
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn config(base: &str, systemone_on: bool) -> Value {
    let systemone = if systemone_on {
        json!({"provider": "openai", "config": {
            "model": "classify-me",
            "api_key": "sk-a",
            "openai_base_url": format!("{base}/v1"),
        }})
    } else {
        json!({"provider": "", "config": {}})
    };
    json!({
        "version": "v1.1",
        "llm": {"provider": "openai", "config": {
            "model": "gpt-4o", "api_key": "sk-a", "openai_base_url": format!("{base}/v1"),
        }},
        "embedder": {"provider": "openai", "config": {
            "model": "text-embedding-3-small", "api_key": "sk-a",
            "openai_base_url": format!("{base}/v1"), "embedding_dims": 10,
        }},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": systemone,
    })
}

async fn add(app: &Router, text: &str) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": text}],
            "user_id": "alice",
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["results"][0].clone()
}

/// Poll `/systemone/counts` until nothing is waiting to be classified.
async fn wait_for_idle(app: &Router) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut last = Value::Null;
    while std::time::Instant::now() < deadline {
        let (_, counts) = call(app, "GET", "/systemone/counts", None).await;
        if counts["pending"] == json!(0) {
            return counts;
        }
        last = counts;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the queue never drained: {last}");
}

#[tokio::test]
async fn a_memory_is_classified_after_the_add_answered() {
    let (base, _up) = start(false).await;
    let a = app();
    call(&a.router, "POST", "/configure", Some(config(&base, true))).await;

    let added = add(&a.router, "a fact worth tagging").await;
    // The trade this feature is: the response no longer carries tags.
    assert_eq!(added["categories"], json!([]), "{added}");

    // ...and the worker picked it up: the tag is on the row a moment later.
    let id = added["id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut one = Value::Null;
    while std::time::Instant::now() < deadline {
        let (_, seen) = call(&a.router, "GET", &format!("/memories/{id}"), None).await;
        one = seen;
        if one["categories"] == json!(["food"]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(one["categories"], json!(["food"]), "{one}");

    let counts = wait_for_idle(&a.router).await;
    assert_eq!(counts["pending"], json!(0), "{counts}");
    assert_eq!(
        counts["classified"],
        json!(1),
        "the worker should report what it got through: {counts}"
    );
}

#[tokio::test]
async fn a_broken_classifier_leaves_the_memory_untagged_and_is_not_retried() {
    let (base, up) = start(true).await;
    let a = app();
    call(&a.router, "POST", "/configure", Some(config(&base, true))).await;

    let (status, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "stored despite the classifier"}],
            "user_id": "alice",
            "infer": false,
        })),
    )
    .await;
    // A classifier that answers 500 must not fail the add — that was true before this change
    // and has to stay true now that the call happens later.
    assert_eq!(status, StatusCode::OK, "{added}");
    let id = added["results"][0]["id"].as_str().unwrap().to_string();

    let counts = wait_for_idle(&a.router).await;
    // The failure is recorded for the operator rather than swallowed...
    assert!(
        counts["last_error"]
            .as_str()
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "{counts}"
    );
    // ...the memory is still there, untagged...
    let (_, one) = call(&a.router, "GET", &format!("/memories/{id}"), None).await;
    assert_eq!(one["categories"], json!([]), "{one}");
    // ...and it is counted as unclassified, which is what the 分类 button works off.
    assert_eq!(counts["unclassified"], json!(1), "{counts}");

    // No retry loop: one failure is one attempt. Give a would-be retry a chance to show up.
    let attempts = up.attempts.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        up.attempts.load(Ordering::SeqCst),
        attempts,
        "a failing classifier must not be retried in a loop"
    );
    assert_eq!(attempts, 1, "one queued memory, one attempt");
}

#[tokio::test]
async fn an_unconfigured_classifier_queues_nothing() {
    let (base, up) = start(false).await;
    let a = app();
    call(&a.router, "POST", "/configure", Some(config(&base, false))).await;

    add(&a.router, "nothing will tag this").await;
    let counts = wait_for_idle(&a.router).await;
    assert_eq!(counts["pending"], json!(0), "{counts}");
    assert_eq!(
        counts["classified"],
        json!(0),
        "no classifier, no background work: {counts}"
    );
    assert_eq!(
        up.attempts.load(Ordering::SeqCst),
        0,
        "and no request to a classifier that is not configured"
    );
}

#[tokio::test]
async fn the_request_keeps_its_classify_segment_at_zero() {
    let (base, _up) = start(false).await;
    let a = app();
    call(&a.router, "POST", "/configure", Some(config(&base, true))).await;

    add(&a.router, "a fact with a stage breakdown").await;
    let (_, rows) = call(&a.router, "GET", "/requests", None).await;
    let add_row = rows
        .as_array()
        .expect("an array of rows")
        .iter()
        .find(|r| r["operation"] == json!("ADD"))
        .expect("an ADD row");
    let stages = &add_row["stages"];
    assert!(
        stages.get("classify_ms").is_some(),
        "the segment stays in the shape: {stages}"
    );
    assert_eq!(
        stages["classify_ms"].as_f64().unwrap_or(-1.0),
        0.0,
        "classification is not inside the request: {stages}"
    );
}
