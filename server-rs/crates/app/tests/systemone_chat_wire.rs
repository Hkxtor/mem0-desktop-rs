//! The exact request a chat classifier sends, and what it does with a bad answer.
//!
//! Every assertion here comes from a real failure against an OpenAI-compatible reasoning
//! model: `top_p: 0.1` (the fork's default) made it think until the budget ran out and
//! answer `content: null`; `response_format: {"type":"json_object"}` made it emit a
//! repeating object; and both failure modes looked like "no category applies" because an
//! empty answer parsed to an empty list.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

#[derive(Clone)]
struct Mock {
    seen: Arc<Mutex<Vec<Value>>>,
    /// What the fake model puts in `choices[0].message.content`.
    content: Arc<Mutex<Value>>,
}

async fn handler(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    mock.seen.lock().unwrap().push(body);
    let content = mock.content.lock().unwrap().clone();
    Json(json!({
        "choices": [{"message": {"role": "assistant", "content": content},
                     "finish_reason": "stop", "index": 0}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }))
}

async fn start(content: Value) -> (Mock, String) {
    let mock = Mock {
        seen: Arc::new(Mutex::new(Vec::new())),
        content: Arc::new(Mutex::new(content)),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(handler))
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
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn config(base_url: &str, extra: Value) -> Value {
    let mut systemone_config = json!({
        "model": "sensenova-6.8-flash",
        "api_key": "sk-test",
        "openai_base_url": base_url,
    });
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            systemone_config[k] = v.clone();
        }
    }
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": {"provider": "openai", "config": systemone_config},
    })
}

#[tokio::test]
async fn chat_classifier_sends_neutral_sampling_and_no_response_format() {
    let (mock, url) = start(json!("[\"technology\", \"food\"]")).await;
    let a = app();
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&url, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, classified) = call(
        &a.router,
        "POST",
        "/systemone/classify",
        Some(json!({"text": "I debugged Rust and ate ramen"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{classified}");
    assert_eq!(
        classified["categories"],
        json!(["technology", "food"]),
        "{classified}"
    );

    let seen = mock.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0];
    // top_p 1.0, not the fork's 0.1 default: a reasoning model sent 0.1 never stops.
    assert_eq!(sent["top_p"], json!(1.0), "{sent}");
    // no response_format: json_object made a real model emit a repeating object.
    assert!(sent.get("response_format").is_none(), "{sent}");
    assert_eq!(sent["temperature"], json!(0.0), "{sent}");
    // 8192 by default: reasoning tokens count against the budget.
    assert_eq!(sent["max_tokens"], json!(8192), "{sent}");
}

#[tokio::test]
async fn chat_classifier_honours_a_configured_max_tokens() {
    let (mock, url) = start(json!("[\"food\"]")).await;
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&url, json!({"max_tokens": 512}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        &a.router,
        "POST",
        "/systemone/classify",
        Some(json!({"text": "ramen"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen.lock().unwrap()[0]["max_tokens"], json!(512));
}

#[tokio::test]
async fn an_empty_answer_is_an_error_not_an_empty_list() {
    // Exactly what a reasoning model does when it runs out of budget: content is null.
    let (_mock, url) = start(Value::Null).await;
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&url, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &a.router,
        "POST",
        "/systemone/classify",
        Some(json!({"text": "anything"})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("empty answer"), "{detail}");
    assert!(detail.contains("max_tokens"), "{detail}");

    // ...and it is recorded, so the Categories page can show it.
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert!(counts["last_error"]
        .as_str()
        .unwrap_or_default()
        .contains("empty answer"));
}

#[tokio::test]
async fn add_survives_a_failing_classifier_but_records_it() {
    let (_mock, url) = start(Value::Null).await;
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&url, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(
            json!({"messages": [{"role": "user", "content": "a rust fact"}],
                    "user_id": "u", "infer": false}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["results"][0]["categories"], json!([]), "{added}");

    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert!(!counts["last_error"].is_null(), "{counts}");
}
