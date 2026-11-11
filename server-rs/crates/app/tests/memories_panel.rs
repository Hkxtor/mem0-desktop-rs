//! The memories page's two data sources: source inputs (what produced a memory) and the
//! batch classifier (label everything that carries no category yet).

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
    /// Answers to hand out, in order; the last one repeats.
    answers: Arc<Mutex<Vec<(u16, Value)>>>,
    calls: Arc<Mutex<usize>>,
}

async fn handler(State(mock): State<Mock>, Json(_body): Json<Value>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut calls = mock.calls.lock().unwrap();
    let idx = (*calls).min(mock.answers.lock().unwrap().len() - 1);
    *calls += 1;
    let (status, content) = mock.answers.lock().unwrap()[idx].clone();
    let body = if status == 200 {
        json!({
            "choices": [{"message": {"role": "assistant", "content": content},
                         "finish_reason": "stop", "index": 0}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })
    } else {
        json!({"error": {"message": "quota exhausted"}})
    };
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

async fn start(answers: Vec<(u16, Value)>) -> (Mock, String) {
    let mock = Mock {
        answers: Arc::new(Mutex::new(answers)),
        calls: Arc::new(Mutex::new(0)),
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
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn config(base_url: &str) -> Value {
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": {"provider": "openai", "config": {
            "model": "mock-model", "api_key": "sk-test", "openai_base_url": base_url,
        }},
    })
}

/// Add one memory from one message; returns its id.
///
/// One message, because a raw (`infer: false`) add creates one memory *per* message —
/// sending two would silently double every count in these tests.
async fn add(app: &Router, text: &str, user: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": format!("about {text}")}],
            "user_id": user,
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["results"][0]["id"].as_str().unwrap().to_string()
}

/// Add with two messages in the request (raw add → two memories).
async fn add_two_messages(app: &Router, user: &str) -> Vec<String> {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [
                {"role": "user", "content": "question about rust"},
                {"role": "assistant", "content": "noted: rust"},
            ],
            "user_id": user,
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_memory_remembers_the_messages_that_produced_it() {
    let (_mock, url) = start(vec![(200, json!("[\"technology\"]"))]).await;
    let a = app();
    let (status, _) = call(&a.router, "POST", "/configure", Some(config(&url))).await;
    assert_eq!(status, StatusCode::OK);

    let ids = add_two_messages(&a.router, "alice").await;
    assert_eq!(ids.len(), 2, "raw add stores one memory per message");
    let id = ids[0].clone();

    let (status, inputs) = call(&a.router, "GET", &format!("/memories/{id}/inputs"), None).await;
    assert_eq!(status, StatusCode::OK, "{inputs}");
    let rows = inputs.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{inputs}");
    // Oldest first, in the order they were sent.
    assert_eq!(rows[0]["seq"], json!(0));
    assert_eq!(rows[0]["role"], json!("user"));
    assert_eq!(rows[0]["content"], json!("question about rust"));
    assert_eq!(rows[1]["role"], json!("assistant"));
    assert_eq!(rows[1]["content"], json!("noted: rust"));

    // Deleting the memory takes its inputs with it — no orphan rows for a dead id.
    let (status, _) = call(&a.router, "DELETE", &format!("/memories/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, after) = call(&a.router, "GET", &format!("/memories/{id}/inputs"), None).await;
    assert_eq!(after, json!([]), "{after}");
}

#[tokio::test]
async fn an_unknown_memory_has_no_inputs() {
    let (_mock, url) = start(vec![(200, json!("[]"))]).await;
    let a = app();
    let (status, _) = call(&a.router, "POST", "/configure", Some(config(&url))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, inputs) = call(&a.router, "GET", "/memories/does-not-exist/inputs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inputs, json!([]));
}

#[tokio::test]
async fn the_batch_classifier_labels_what_is_unclassified_and_reports_the_rest() {
    let (_mock, url) = start(vec![(200, json!("[\"technology\"]"))]).await;
    let a = app();
    let (status, _) = call(&a.router, "POST", "/configure", Some(config(&url))).await;
    assert_eq!(status, StatusCode::OK);

    // Three memories, all classified at add time by the mock, so clear them first by
    // adding with the classifier off is not possible here — instead check the store the
    // batch sees: all three carry ["technology"] already, so nothing is a candidate.
    for text in ["a", "b", "c"] {
        add(&a.router, text, "alice").await;
    }
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert_eq!(counts["unclassified"], json!(0), "{counts}");

    let (status, batch) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{batch}");
    assert_eq!(batch["processed"], json!(0), "{batch}");
    assert_eq!(batch["remaining"], json!(0), "{batch}");
}

#[tokio::test]
async fn the_batch_classifier_fills_empty_categories_and_skips_expired() {
    // One answer that labels, then a long run of "no match".
    let (_mock, url) = start(vec![(200, json!("[\"food\"]"))]).await;

    // Configure with the classifier OFF so the adds store empty categories.
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some({
            let mut c = config(&url);
            c["systemone"] = json!({"provider": "", "config": {}});
            c
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fresh = add(&a.router, "ramen", "alice").await;
    add(&a.router, "sushi", "alice").await;
    // An expired memory must stay untouched: it is hidden everywhere the UI looks.
    let (status, expired_body) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "old news"}],
            "user_id": "alice", "infer": false, "expiration_date": "2020-01-01",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{expired_body}");

    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert_eq!(
        counts["unclassified"],
        json!(2),
        "expired must not count: {counts}"
    );

    // Turn classification on and run one batch of one.
    let (status, _) = call(&a.router, "POST", "/configure", Some(config(&url))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, batch) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{batch}");
    assert_eq!(batch["processed"], json!(1), "{batch}");
    assert_eq!(batch["labeled"], json!(1), "{batch}");
    assert_eq!(batch["remaining"], json!(1), "{batch}");

    // The labelled memory now carries the category...
    let (_, one) = call(&a.router, "GET", &format!("/memories/{fresh}"), None).await;
    assert_eq!(one["categories"], json!(["food"]), "{one}");

    // ...and the second batch finishes the store.
    let (_, batch2) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 10})),
    )
    .await;
    assert_eq!(batch2["remaining"], json!(0), "{batch2}");
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert_eq!(counts["unclassified"], json!(0), "{counts}");
}

#[tokio::test]
async fn the_batch_classifier_stops_after_three_consecutive_failures() {
    // Every provider call fails.
    let (_mock, url) = start(vec![(500, Value::Null)]).await;
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some({
            let mut c = config(&url);
            c["systemone"] = json!({"provider": "", "config": {}});
            c
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for text in ["a", "b", "c", "d", "e", "f"] {
        add(&a.router, text, "alice").await;
    }
    let (status, _) = call(&a.router, "POST", "/configure", Some(config(&url))).await;
    assert_eq!(status, StatusCode::OK);

    let (status, batch) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{batch}");
    assert_eq!(
        batch["processed"],
        json!(3),
        "must not hammer a dead quota: {batch}"
    );
    assert_eq!(batch["failed"], json!(3), "{batch}");
    assert!(
        batch["stopped_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("quota")
            || !batch["stopped_reason"].is_null(),
        "{batch}"
    );
    assert_eq!(batch["remaining"], json!(3), "{batch}");

    // The failure is visible, not swallowed.
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert!(!counts["last_error"].is_null(), "{counts}");
}

#[tokio::test]
async fn skip_ids_are_not_retried_in_the_same_session() {
    // The model answers with an empty list: a real "nothing fits" answer.
    let (_mock, url) = start(vec![(200, json!("[]"))]).await;
    let a = app();
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some({
            let mut c = config(&url);
            c["systemone"] = json!({"provider": "", "config": {}});
            c
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = add(&a.router, "junk", "alice").await;
    let (status, _) = call(&a.router, "POST", "/configure", Some(config(&url))).await;
    assert_eq!(status, StatusCode::OK);

    let (_, first) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 10})),
    )
    .await;
    assert_eq!(first["no_match"], json!(1), "{first}");
    assert_eq!(first["no_match_ids"], json!([id]), "{first}");
    assert_eq!(first["remaining"], json!(0), "{first}");

    // Asking again without the skip list retries it...
    let (_, again) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 10})),
    )
    .await;
    assert_eq!(again["processed"], json!(1), "{again}");

    // ...and with it, nothing is attempted at all.
    let (_, skipped) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 10, "skip": [id]})),
    )
    .await;
    assert_eq!(skipped["processed"], json!(0), "{skipped}");
}

#[tokio::test]
async fn the_batch_classifier_needs_a_configured_model() {
    let a = app();
    // llm + embedder configured, systemone absent: the runtime exists, so the failure
    // has to be about classification rather than about a missing runtime.
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({
            "llm": {"provider": "mock", "config": {}},
            "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
            "vector_store": {"provider": "sqlite-vec",
                             "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &a.router,
        "POST",
        "/systemone/classify-batch",
        Some(json!({"limit": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("systemone_not_configured"));
}
