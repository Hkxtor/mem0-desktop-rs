//! Regression: the Requests and Analytics pages were empty because nothing was logged.
//!
//! `StateStore::log_request` existed but had no caller, so `request_logs` stayed empty
//! and `/requests` returned `[]` while `/analytics/summary` reported 0 operations. Field
//! report was "memories have data but requests/analytics do not".

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// Keeps the temp data directory alive for the lifetime of the test.
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

/// Same, but with a memory runtime built from the mock providers, so the memory
/// endpoints actually run instead of answering 503.
async fn app_with_memory() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    let cfg = serde_json::json!({
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "history_db_path": ""
    });
    let memory = std::sync::Arc::new(
        mem0_desktop_rs::memory_bridge::build_memory(&cfg, dir.path()).expect("mock build"),
    );
    let router = mem0_desktop_rs::app_for_test_with_memory(dir.path().to_path_buf(), memory);
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

/// The page lists what happened to the memories, and nothing else.
#[tokio::test]
async fn requests_lists_memory_operations_not_routes() {
    let a = app_with_memory().await;
    let app = &a.router;

    // Traffic that is not a memory operation: it must not show up on the page.
    call(app, "GET", "/health", None).await;
    call(app, "GET", "/configure", None).await;

    // Real memory operations.
    let (status, added) = call(
        app,
        "POST",
        "/memories",
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "Alice likes hiking"}],
            "user_id": "alice",
            "infer": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    let id = added["results"][0]["id"].as_str().unwrap().to_string();

    call(
        app,
        "POST",
        "/search",
        Some(serde_json::json!({"query": "hiking", "filters": {"user_id": "alice"}, "top_k": 5})),
    )
    .await;
    call(
        app,
        "PUT",
        &format!("/memories/{id}"),
        Some(serde_json::json!({"data": "Alice enjoys hiking"})),
    )
    .await;

    let (status, body) = call(app, "GET", "/requests", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap_or_else(|| panic!("rows: {body}"));
    let operations: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["operation"].as_str())
        .collect();
    assert_eq!(
        operations,
        vec!["UPDATE", "SEARCH", "ADD"],
        "newest first, memory actions only: {body}"
    );

    // Shape the dashboard reads; a missing field crashed the page before.
    let row = &rows[0];
    for key in [
        "id",
        "operation",
        "detail",
        "status_code",
        "latency_ms",
        "created_at",
    ] {
        assert!(!row[key].is_null(), "{key} missing from {row}");
    }
    assert!(row["id"].is_string(), "id is a string: {row}");
    assert_eq!(
        rows[1]["detail"],
        serde_json::json!("\"hiking\" · user:alice")
    );
    assert_eq!(rows[1]["results"], serde_json::json!(1));
    assert_eq!(rows[2]["detail"], serde_json::json!("user:alice"));
    assert_eq!(rows.iter().filter(|r| r["operation"] == "ADD").count(), 1);
    // Route-level traffic stays out of the operation log.
    assert!(
        !operations.iter().any(|o| o.contains('/')),
        "no HTTP paths on this page: {operations:?}"
    );
}

/// A failed operation is still recorded, with its status.
#[tokio::test]
async fn failed_operations_are_recorded_with_their_status() {
    let a = app_with_memory().await;
    let app = &a.router;
    let (status, _) = call(
        app,
        "POST",
        "/memories",
        Some(serde_json::json!({"messages": [{"role": "user", "content": "x"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no identifier given");
    let (_, body) = call(app, "GET", "/requests", None).await;
    let rows = body.as_array().cloned().unwrap_or_default();
    assert_eq!(rows[0]["operation"], serde_json::json!("ADD"), "{body}");
    assert_eq!(rows[0]["status_code"], serde_json::json!(400), "{body}");
}

/// Analytics still reports HTTP traffic; that is a different question and stays separate.
#[tokio::test]
async fn analytics_still_counts_served_requests() {
    let a = app();
    let app = &a.router;
    call(app, "GET", "/health", None).await;
    call(app, "GET", "/memories", None).await;
    let (status, summary) = call(app, "GET", "/analytics/summary", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        summary["operations"].as_i64().unwrap_or(0) >= 1,
        "analytics counts served requests: {summary}"
    );
    // Python always returns a full window of points; gaps read as missing data.
    assert!(
        summary["daily"].as_array().map(|d| d.len()).unwrap_or(0) > 1,
        "daily series padded to the window: {summary}"
    );
}

#[tokio::test]
async fn the_port_setting_round_trips_and_reports_what_is_bound() {
    let a = app();
    let app = &a.router;

    let (status, body) = call(app, "GET", "/settings/server", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("port").is_some(), "shape: {body}");
    assert!(body.get("bound_port").is_some(), "shape: {body}");

    let (status, body) = call(
        app,
        "PUT",
        "/settings/server",
        Some(serde_json::json!({"port": 8901})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["port"], serde_json::json!(8901));
    assert_eq!(body["requires_restart"], serde_json::json!(true));

    let (_, body) = call(app, "GET", "/settings/server", None).await;
    assert_eq!(
        body["port"],
        serde_json::json!(8901),
        "the saved port must be reported back: {body}"
    );

    // Clearing it returns to automatic selection.
    let (status, _) = call(
        app,
        "PUT",
        "/settings/server",
        Some(serde_json::json!({"port": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(app, "GET", "/settings/server", None).await;
    assert!(body["port"].is_null(), "{body}");

    // A privileged port is refused with the reason, not bound and failed later.
    let (status, body) = call(
        app,
        "PUT",
        "/settings/server",
        Some(serde_json::json!({"port": 80})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
