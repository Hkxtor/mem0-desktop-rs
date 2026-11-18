//! A rejected configuration must not change what the process is running.
//!
//! `set_config` used to install the new `Memory` first and only then build the classifier and
//! write the file, so a save that returned 400 left the process on the new configuration while
//! `config.json` still held the old one. `/configure` GET and the actual behaviour disagreed
//! until a restart — the worst kind of state to debug.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    router: axum::Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    let router = mem0_desktop_rs::app_for_test(dir.path().to_path_buf());
    App { _dir: dir, router }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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

fn working() -> Value {
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec",
                         "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    })
}

/// The live classifier: what the process would actually use, not what the file says.
async fn live_model(app: &axum::Router) -> Value {
    let (_, body) = call(app, "GET", "/systemone/counts", None).await;
    body["model"].clone()
}

#[tokio::test]
async fn a_rejected_classifier_leaves_the_running_configuration_alone() {
    let a = app();
    let (status, body) = call(&a.router, "POST", "/configure", Some(working())).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A valid systemone configuration, so there is a live classifier to protect.
    let mut with_classifier = working();
    with_classifier["systemone"] = json!({
        "provider": "systemone",
        "config": {"api_key": "k", "model": "gemini-decision-1"},
    });
    let (status, body) = call(&a.router, "POST", "/configure", Some(with_classifier)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(live_model(&a.router).await, json!("gemini-decision-1"));

    // Now a save that builds the Memory fine but whose classifier cannot be built.
    let mut broken = working();
    broken["systemone"] = json!({
        "provider": "systemone",
        // Out of range: the decision client refuses to build from this.
        "config": {"api_key": "k", "model": "gemini-decision-1", "noul_threshold": 5.0},
    });
    let (status, body) = call(&a.router, "POST", "/configure", Some(broken)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The running classifier is still the old one, not the rejected one and not nothing.
    assert_eq!(
        live_model(&a.router).await,
        json!("gemini-decision-1"),
        "a rejected save must not change what is running"
    );

    // And the file agrees with the process.
    let (_, stored) = call(&a.router, "GET", "/configure", None).await;
    let stored_model = stored["systemone"]["config"]["model"].clone();
    assert_eq!(stored_model, json!("gemini-decision-1"), "{stored}");
}

/// A save that cannot even build the Memory must leave everything alone too.
#[tokio::test]
async fn a_rejected_memory_build_leaves_the_running_configuration_alone() {
    let a = app();
    let (status, _) = call(&a.router, "POST", "/configure", Some(working())).await;
    assert_eq!(status, StatusCode::OK);

    let mut broken = working();
    broken["llm"] = json!({"provider": "no-such-provider", "config": {}});
    let (status, _) = call(&a.router, "POST", "/configure", Some(broken)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, stored) = call(&a.router, "GET", "/configure", None).await;
    assert_eq!(stored["llm"]["provider"], json!("mock"), "{stored}");
}
