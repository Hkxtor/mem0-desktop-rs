//! The Gemini vendor of the decision backend, against a local mock.
//!
//! Google's decision API is the same *shape* as StartLux's — one input, typed questions,
//! one answer each — but differs everywhere that matters: the RPC path, the auth header,
//! the field that carries the text, and the meaning of the `noul` number. A test that only
//! checked the parsed categories would pass with the polarity inverted, so these tests
//! assert the outgoing request and read the real answer shape back.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Mock {
    answers: Arc<Mutex<Value>>,
    seen: Arc<Mutex<Vec<Value>>>,
    goog_key: Arc<Mutex<Vec<Option<String>>>>,
    authorization: Arc<Mutex<Vec<Option<String>>>>,
    status: Arc<Mutex<u16>>,
    error_body: Arc<Mutex<Option<Value>>>,
}

async fn handler(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    mock.seen.lock().unwrap().push(body);
    mock.goog_key.lock().unwrap().push(
        headers
            .get("x-goog-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    );
    mock.authorization.lock().unwrap().push(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    );
    let status = StatusCode::from_u16(*mock.status.lock().unwrap()).unwrap();
    if let Some(err) = mock.error_body.lock().unwrap().clone() {
        return (status, Json(err));
    }
    let answers = mock.answers.lock().unwrap().clone();
    (status, Json(json!({ "answers": answers })))
}

/// Stand a mock in for `generativelanguage.googleapis.com`. Returns the *base* URL, so the
/// client has to append `/v1beta/decisions` itself — the mock only answers that path.
async fn start(answers: Value, status: u16, error_body: Option<Value>) -> (Mock, String) {
    let mock = Mock {
        answers: Arc::new(Mutex::new(answers)),
        status: Arc::new(Mutex::new(status)),
        error_body: Arc::new(Mutex::new(error_body)),
        ..Default::default()
    };
    let app = Router::new()
        .route("/v1beta/decisions", post(handler))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (mock, format!("http://{addr}"))
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

fn gemini_config(base_url: &str) -> Value {
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": {
            "provider": "systemone",
            "config": {"vendor": "gemini", "api_key": "AIza-test-key", "base_url": base_url},
        },
    })
}

/// A high `noul` means the category applies. This is the assertion that fails if the
/// StartLux comparison is reused: the same answers would produce the complement set.
#[tokio::test]
async fn a_high_noul_tags_the_category() {
    let (mock, base) = start(
        json!({
            "technology": {"type": "noul", "noul": 0.83, "confidence": 0.9},
            "food": {"type": "noul", "noul": 0.76, "confidence": 0.9},
            "travel": {"type": "noul", "noul": 0.04, "confidence": 0.99},
        }),
        200,
        None,
    )
    .await;

    let a = app();
    let (status, body) = call(&a.router, "POST", "/configure", Some(gemini_config(&base))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I cooked ramen while debugging Rust"}],
            "user_id": "u", "infer": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(
        added["results"][0]["categories"],
        json!(["food", "technology"]),
        "{added}"
    );

    // The wire: Gemini's path, Gemini's auth header, Gemini's field name, no think_mode.
    let seen = mock.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let sent = &seen[0];
    assert_eq!(
        sent["input"],
        json!("I cooked ramen while debugging Rust"),
        "{sent}"
    );
    assert_eq!(sent["model"], json!("gemini-decision-1"), "{sent}");
    assert!(
        sent.get("state").is_none(),
        "`state` is StartLux's field: {sent}"
    );
    assert!(
        sent.get("think_mode").is_none(),
        "Gemini 400s on unknown fields: {sent}"
    );
    assert_eq!(
        sent["questions"].as_object().map(|q| q.len()),
        Some(mem0_desktop_rs::categories::PRESET_CATEGORIES.len())
    );
    assert_eq!(sent["questions"]["food"]["type"], json!("noul"));

    // The key travels in Google's header, not as a bearer token.
    assert_eq!(
        mock.goog_key.lock().unwrap()[0],
        Some("AIza-test-key".to_string())
    );
    assert_eq!(
        mock.authorization.lock().unwrap()[0],
        None,
        "Gemini rejects a bearer token"
    );

    // `/systemone/test` reports the effective model, not the empty field the operator left
    // blank — "model: """ reads as broken when it is a working default.
    let (status, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(true), "{tested}");
    assert_eq!(tested["model"], json!("gemini-decision-1"), "{tested}");
    assert_eq!(
        tested["raw"]["food"]["noul"],
        json!(0.76),
        "the raw probabilities are what makes a threshold diagnosable: {tested}"
    );
}

/// A provider error has to reach the operator as the provider's sentence, not as a blob of
/// JSON: `/systemone/test` is where a misconfigured model gets diagnosed.
#[tokio::test]
async fn googles_error_message_is_what_the_operator_sees() {
    let (_mock, base) = start(
        json!({}),
        404,
        Some(json!({
            "error": {
                "code": 404,
                "message": "Requested model not found. See https://ai.google.dev/gemini-api/docs/models",
                "status": "NOT_FOUND"
            }
        })),
    )
    .await;

    let a = app();
    let (status, body) = call(&a.router, "POST", "/configure", Some(gemini_config(&base))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(false), "{tested}");
    let error = tested["error"].as_str().unwrap_or_default();
    assert!(error.contains("Requested model not found"), "{tested}");
    assert!(
        error.contains("404"),
        "the status has to be visible: {tested}"
    );
    assert!(
        !error.contains("\"status\""),
        "the raw envelope is noise: {tested}"
    );
}

/// A classification failure must not fail the add: the memory is stored untagged and the
/// reason is recorded where the UI can show it.
#[tokio::test]
async fn a_rejected_key_still_stores_the_memory_untagged() {
    let (mock, base) = start(
        json!({}),
        400,
        Some(json!({
            "error": {
                "code": 400,
                "message": "API key not valid. Please pass a valid API key.",
                "status": "INVALID_ARGUMENT"
            }
        })),
    )
    .await;

    let a = app();
    let (status, body) = call(&a.router, "POST", "/configure", Some(gemini_config(&base))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I love ramen"}],
            "user_id": "u", "infer": false
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an untagged memory is still a success: {added}"
    );
    assert_eq!(added["results"][0]["categories"], json!([]), "{added}");
    assert_eq!(mock.seen.lock().unwrap().len(), 1);

    // And the failure is visible rather than swallowed.
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    let last_error = counts["last_error"].as_str().unwrap_or_default();
    assert!(last_error.contains("API key not valid"), "{counts}");
}
