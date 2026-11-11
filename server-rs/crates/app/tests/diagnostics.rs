//! `GET /diagnostics/storage` over HTTP: the shape the two memory pages rely on.
//!
//! The pages call this only when their list came back empty, so what matters here is that
//! the answer distinguishes "nothing was ever added" from "this app cannot read what is
//! there" — an empty page with the wrong explanation sends the operator looking in the
//! wrong place.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    router: axum::Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let router = mem0_desktop_rs::app_for_test(path.clone());
    App {
        _dir: dir,
        dir: path,
        router,
    }
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
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn configure_body() -> Value {
    json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    })
}

#[tokio::test]
async fn a_fresh_install_says_no_provider_is_configured() {
    let a = app();
    let (status, body) = call(&a.router, "GET", "/diagnostics/storage", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["runtime_configured"], json!(false));
    assert_eq!(body["has_memories"], json!(false));
    assert_eq!(body["data_dir"], json!(a.dir.display().to_string()));
    let hint = body["hint"].as_str().unwrap();
    assert!(hint.contains("/setup"), "{hint}");
}

#[tokio::test]
async fn an_empty_but_configured_store_reports_the_directory() {
    let a = app();
    call(&a.router, "POST", "/configure", Some(configure_body())).await;
    let (status, body) = call(&a.router, "GET", "/diagnostics/storage", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["runtime_configured"], json!(true));
    assert_eq!(body["has_memories"], json!(false));
    let hint = body["hint"].as_str().unwrap();
    assert!(hint.contains("MEM0_DATA_DIR"), "{hint}");
}

#[tokio::test]
async fn stored_memories_clear_the_hint() {
    let a = app();
    call(&a.router, "POST", "/configure", Some(configure_body())).await;
    let (status, _) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I like hiking"}],
            "user_id": "alice",
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(&a.router, "GET", "/diagnostics/storage", None).await;
    assert_eq!(body["has_memories"], json!(true), "{body}");
    assert!(body["hint"].is_null(), "{body}");
}

#[tokio::test]
async fn python_build_data_is_reported_as_unreadable_rather_than_absent() {
    let a = app();
    // The Python build writes data/duckdb/<collection>.db; this app reads vectors.db.
    std::fs::create_dir_all(a.dir.join("duckdb")).unwrap();
    std::fs::write(a.dir.join("duckdb").join("mem0.db"), b"not a sqlite file").unwrap();
    call(&a.router, "POST", "/configure", Some(configure_body())).await;

    let (status, body) = call(&a.router, "GET", "/diagnostics/storage", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["legacy_python_data"], json!(["duckdb/mem0.db"]));
    assert_eq!(body["has_memories"], json!(false));
    let hint = body["hint"].as_str().unwrap();
    assert!(hint.contains("duckdb/mem0.db"), "{hint}");
    assert!(hint.contains("import"), "{hint}");
}
