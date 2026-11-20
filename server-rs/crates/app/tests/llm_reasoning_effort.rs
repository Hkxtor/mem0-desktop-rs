//! `llm.config.reasoning_effort`: how hard a reasoning model is allowed to think.
//!
//! The field is opt-in, exactly like `input_type`. Absent means the client does not send it and
//! the provider applies its own default — which is what every existing install gets, so adding
//! the setting must not change a single request for anyone who leaves it alone.
//!
//! The client only sends it for models it recognises as reasoning models (`o1`, `o3`, `gpt-5`…),
//! so a `gpt-4o` install that happens to have the key stored still sends nothing. These tests
//! read the bodies the LLM actually sent to a stand-in upstream, so "not sent" means the key is
//! absent from the wire, not merely unset in a struct.

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
    /// Every chat request body, in order.
    bodies: Arc<Mutex<Vec<Value>>>,
}

async fn chat(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    mock.bodies.lock().unwrap().push(body);
    // A minimal extraction answer: one memory, so the add completes and the request is recorded.
    Json(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": json!({"memory": [{"id": "0", "text": "a remembered fact"}]}).to_string(),
            },
            "finish_reason": "stop",
            "index": 0,
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }))
}

async fn embeddings(Json(body): Json<Value>) -> Json<Value> {
    let n = body["input"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(1)
        .max(1);
    let data: Vec<Value> = (0..n)
        .map(|i| json!({"embedding": vec![0.5f32; 8], "index": i}))
        .collect();
    Json(json!({ "data": data, "usage": {"total_tokens": 1} }))
}

async fn start() -> (Mock, String) {
    let mock = Mock::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
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
    base: String,
}

fn app(base: &str) -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    let router = mem0_desktop_rs::app_for_test(dir.path().to_path_buf());
    App {
        _dir: dir,
        router,
        base: base.to_string(),
    }
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

fn config(base_url: &str, model: &str, effort: Option<&str>) -> Value {
    let mut llm = json!({
        "provider": "openai",
        "config": {
            "model": model,
            "api_key": "sk-a",
            "openai_base_url": base_url,
        },
    });
    if let Some(effort) = effort {
        llm["config"]["reasoning_effort"] = json!(effort);
    }
    json!({
        "version": "v1.1",
        "llm": llm,
        "embedder": {"provider": "openai", "config": {
            "model": "text-embedding-3-small",
            "api_key": "sk-a",
            "openai_base_url": base_url,
            "embedding_dims": 8,
        }},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
    })
}

/// One add, so the LLM is actually called once.
async fn add_extracting(app: &Router) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I like tea"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn a_reasoning_model_is_told_how_hard_to_think() {
    let (mock, base) = start().await;
    let a = app(&base);
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&a.base, "gpt-5", Some("low"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    add_extracting(&a.router).await;

    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert_eq!(
        bodies[0]["reasoning_effort"],
        json!("low"),
        "the chosen effort must reach the wire: {}",
        bodies[0]
    );
}

#[tokio::test]
async fn leaving_it_alone_sends_nothing_at_all() {
    let (mock, base) = start().await;
    let a = app(&base);
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&a.base, "gpt-5", None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    add_extracting(&a.router).await;

    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(
        bodies[0].get("reasoning_effort").is_none(),
        "an unset effort must not appear in the body: {}",
        bodies[0]
    );
}

#[tokio::test]
async fn a_non_reasoning_model_never_sees_the_field() {
    // Storing the key for a chat model — switching models later, say — must not leak a parameter
    // the endpoint may reject.
    let (mock, base) = start().await;
    let a = app(&base);
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&a.base, "gpt-4o", Some("high"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    add_extracting(&a.router).await;

    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(
        bodies[0].get("reasoning_effort").is_none(),
        "gpt-4o does not take reasoning_effort: {}",
        bodies[0]
    );
}

#[tokio::test]
async fn clearing_the_setting_removes_the_key_instead_of_storing_an_empty_string() {
    let (mock, base) = start().await;
    let a = app(&base);
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&a.base, "gpt-5", Some("high"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, saved) = call(&a.router, "GET", "/configure", None).await;
    assert_eq!(saved["llm"]["config"]["reasoning_effort"], json!("high"));

    // The dashboard clears the select by sending an explicit empty string: omitting the key
    // would mean "keep what is stored".
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&a.base, "gpt-5", Some(""))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, saved) = call(&a.router, "GET", "/configure", None).await;
    assert!(
        saved["llm"]["config"].get("reasoning_effort").is_none(),
        "an empty effort must be removed, not stored: {saved}"
    );

    add_extracting(&a.router).await;
    let bodies = mock.bodies.lock().unwrap().clone();
    assert!(
        bodies[0].get("reasoning_effort").is_none(),
        "and the wire agrees: {}",
        bodies[0]
    );
}
