//! HTTP-level tests for the SystemOne decision backend, against a local mock server.
//!
//! The decision API is not chat-completions: it takes a model, a state and a map of
//! typed questions, and returns one answer per question. These tests stand a real axum
//! server in for it, so the wire protocol is asserted rather than just the parsing.

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
    answers: Arc<Mutex<Value>>,
    seen: Arc<Mutex<Vec<Value>>>,
    auth: Arc<Mutex<Vec<Option<String>>>>,
    status: Arc<Mutex<u16>>,
}

async fn handler(
    State(mock): State<Mock>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    mock.seen.lock().unwrap().push(body);
    mock.auth.lock().unwrap().push(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    );
    let status = *mock.status.lock().unwrap();
    let answers = mock.answers.lock().unwrap().clone();
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({"model": "mock-decision", "answers": answers})),
    )
}

async fn start(answers: Value, status: u16) -> (Mock, String) {
    let mock = Mock {
        answers: Arc::new(Mutex::new(answers)),
        seen: Arc::new(Mutex::new(Vec::new())),
        auth: Arc::new(Mutex::new(Vec::new())),
        status: Arc::new(Mutex::new(status)),
    };
    let app = Router::new()
        .route("/v1/systemone", post(handler))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (mock, format!("http://{addr}/v1/systemone"))
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

fn config_with_systemone(base_url: &str) -> Value {
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": {
            "provider": "systemone",
            "config": {
                "model": "startlux-decision-27b-autothink",
                "api_key": "bamboo-test-key",
                "base_url": base_url,
            },
        },
    })
}

#[tokio::test]
async fn decision_backend_tags_memories_from_the_answers_object() {
    // The documented example shape: noul is P(does not apply), so 0.02 means "applies".
    let (mock, url) = start(
        json!({
            "technology": {"type": "noul", "noul": 0.02},
            "food": {"type": "noul", "noul": 0.05},
            "travel": {"type": "noul", "noul": 0.9},
            "sports": {"type": "noul", "noul": 0.8},
        }),
        200,
    )
    .await;

    let a = app();
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config_with_systemone(&url)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(
            json!({"messages": [{"role": "user", "content": "I cooked ramen while debugging Rust"}],
                    "user_id": "u", "infer": false}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(
        added["results"][0]["categories"],
        json!(["food", "technology"]),
        "{added}"
    );

    // protocol shape: one noul question per preset category, bearer auth, state = the text
    let seen = mock.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let body = &seen[0];
    assert_eq!(body["state"], json!("I cooked ramen while debugging Rust"));
    assert_eq!(body["model"], json!("startlux-decision-27b-autothink"));
    assert_eq!(body["think_mode"], json!("auto"));
    assert_eq!(
        body["questions"].as_object().unwrap().len(),
        mem0_desktop_rs::categories::PRESET_CATEGORIES.len()
    );
    assert_eq!(body["questions"]["food"]["type"], json!("noul"));
    assert_eq!(
        mock.auth.lock().unwrap()[0],
        Some("Bearer bamboo-test-key".to_string())
    );

    // the raw answers are what /systemone/test shows
    let (status, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(true), "{tested}");
    assert_eq!(
        tested["categories"],
        json!(["food", "technology"]),
        "{tested}"
    );
    assert_eq!(tested["raw"]["travel"]["noul"], json!(0.9), "{tested}");
}

#[tokio::test]
async fn decision_threshold_filters_weak_answers() {
    let (_mock, url) = start(json!({"technology": {"noul": 0.4}}), 200).await;
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config_with_systemone(&url)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(tested["categories"], json!(["technology"]), "{tested}");

    // a stricter threshold turns the same answer off
    let mut cfg = config_with_systemone(&url);
    cfg["systemone"]["config"]["noul_threshold"] = json!(0.3);
    let (status, _) = call(&a.router, "POST", "/configure", Some(cfg)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(tested["categories"], json!([]), "{tested}");
}

#[tokio::test]
async fn decision_http_error_is_reported_with_the_endpoint() {
    let (_mock, url) = start(json!({}), 404).await;
    let a = app();
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config_with_systemone(&url)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(false), "{tested}");
    let err = tested["error"].as_str().unwrap_or_default();
    assert!(err.contains("404"), "{err}");
    assert!(err.contains("/v1/systemone"), "{err}");

    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert!(counts["last_error"]
        .as_str()
        .unwrap_or_default()
        .contains("404"));
}
