//! The Logs page endpoints, and the reason a zero-result add records.
//!
//! The motivating case: a client adds a memory, gets HTTP 200 with `results: []`, the
//! Requests page shows an ADD with 0 results, and the Entities page is empty. Nothing is
//! wrong with the entity aggregation — nothing was stored. These tests pin the explanation
//! that was missing.

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
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
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
        "vector_store": {"provider": "sqlite-vec", "config": {
            "collection_name": "mem0", "embedding_model_dims": 10
        }},
    })
}

/// The whole point of the feature: a 200 with no memories says *why*.
///
/// The bundled mock LLM answers `{"memory": []}` — a model that looked at the message and
/// found nothing to remember. That is the legitimate zero, and it must be labelled as such
/// rather than left as a bare `0` the operator has to interpret.
#[tokio::test]
async fn a_zero_result_add_records_why() {
    let app = app();
    let (status, _) = call(&app.router, "POST", "/configure", Some(configure_body())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I love ramen"}],
            "user_id": "ou_68c5b20d54c6e9ae42abbd77fd1e251e",
            "agent_id": "hermes",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"], json!([]), "the mock extracts nothing");

    let (status, ops) = call(&app.router, "GET", "/requests?limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = &ops[0];
    assert_eq!(row["operation"], json!("ADD"));
    assert_eq!(row["results"], json!(0));
    assert_eq!(
        row["reason"],
        json!("no_memories"),
        "a zero must explain itself: {row}"
    );
}

/// A successful add carries no reason: the badge is for the inexplicable case only.
#[tokio::test]
async fn a_successful_add_has_no_reason() {
    let app = app();
    let (status, _) = call(&app.router, "POST", "/configure", Some(configure_body())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I love ramen"}],
            "user_id": "alice",
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"].as_array().map(|a| a.len()), Some(1));

    let (_, ops) = call(&app.router, "GET", "/requests?limit=10", None).await;
    assert_eq!(ops[0]["results"], json!(1));
    assert!(ops[0]["reason"].is_null(), "{}", ops[0]);
}

/// The application log is written to the data directory, not to the database, and the
/// endpoint says where it is so the file can be handed to someone else.
#[tokio::test]
async fn the_app_log_endpoint_reports_where_the_file_is() {
    let app = app();
    mem0_desktop_rs::logging::init(&app.dir);

    let (status, body) = call(&app.router, "GET", "/logs/app?limit=50", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["initialised"], json!(true), "{body}");
    let path = body["path"].as_str().unwrap_or_default();
    // The directory is not asserted: the writer is process-global and idempotent, so the
    // first test to initialise it owns the path for the whole binary.
    assert!(path.ends_with("logs/app.log"), "{path}");
    assert!(body["lines"].is_array(), "{body}");
}

/// The request-log tab reads HTTP traffic, which is a different table from the memory
/// operations the Requests page shows.
#[tokio::test]
async fn the_request_log_endpoint_lists_http_traffic() {
    let app = app();
    // One request that is not itself excluded from the log.
    let (status, _) = call(&app.router, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&app.router, "GET", "/configure", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&app.router, "GET", "/logs/requests?limit=50", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["requests"].as_array().cloned().unwrap_or_default();
    assert!(
        !rows.is_empty(),
        "the /configure call should be logged: {body}"
    );
    let paths: Vec<&str> = rows.iter().filter_map(|r| r["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| p.starts_with("/configure")),
        "{paths:?}"
    );
    // Reading the log must not add to it: a page left open would otherwise drown itself.
    assert!(
        !paths.iter().any(|p| p.starts_with("/logs")),
        "the log endpoints must not log themselves: {paths:?}"
    );
}

/// Both endpoints are admin-only. The application log can contain memory content (the
/// model's answer), so it is not something an ordinary user should read.
///
/// The handlers are called directly rather than through a router: turning authentication on
/// is a process-global switch, and a test that flips it would race every other test in this
/// binary. Calling the handler with a viewer context tests the gate itself, which is the
/// part that could regress.
#[tokio::test]
async fn both_log_endpoints_require_admin() {
    use axum::extract::{Query, State};
    use axum::Extension;

    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(
        mem0_desktop_rs::http::AppState::new(dir.path().to_path_buf()).expect("state"),
    );
    let viewer = mem0_desktop_rs::http::auth::AuthContext {
        user_id: Some("u-viewer".into()),
        username: Some("viewer".into()),
        role: Some("viewer".into()),
        auth_type: mem0_desktop_rs::http::auth::AuthType::Bearer,
    };

    let err = mem0_desktop_rs::http::routes_logs::app_log(
        State(state.clone()),
        Extension(viewer.clone()),
        Query(std::collections::HashMap::new()),
    )
    .await
    .expect_err("a viewer must not read the application log");
    assert_eq!(err.status.as_u16(), 403, "{}", err.detail);

    let err = mem0_desktop_rs::http::routes_logs::request_log(
        State(state),
        Extension(viewer),
        Query(std::collections::HashMap::new()),
    )
    .await
    .expect_err("a viewer must not read the request log");
    assert_eq!(err.status.as_u16(), 403, "{}", err.detail);
}

/// Pruning is what keeps the two tables bounded. It must drop old rows and keep recent ones,
/// because the Analytics page reads a 12-day window from the same data.
#[tokio::test]
async fn pruning_drops_old_rows_and_keeps_recent_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = mem0_desktop_rs::store::StateStore::open(&dir.path().join("mem0.db")).unwrap();
    store.log_request("GET", "/old", 200, 1.0, "none").unwrap();
    store.log_request("GET", "/new", 200, 1.0, "none").unwrap();
    store
        .log_memory_operation("ADD", "user:alice", Some(0), 200, 1.0, Some("no_memories"))
        .unwrap();

    // Backdate everything, then add one fresh row per table.
    {
        let conn = rusqlite::Connection::open(dir.path().join("mem0.db")).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        conn.execute("UPDATE request_logs SET created_at=?1", [&old])
            .unwrap();
        conn.execute("UPDATE memory_operations SET created_at=?1", [&old])
            .unwrap();
    }
    store
        .log_request("GET", "/fresh", 200, 1.0, "none")
        .unwrap();
    store
        .log_memory_operation("SEARCH", "hiking", Some(1), 200, 1.0, None)
        .unwrap();

    let (requests, operations) = store.prune_history(14).unwrap();
    assert_eq!(requests, 2, "two old request rows should go");
    assert_eq!(operations, 1, "one old operation row should go");

    let remaining = store.list_request_logs(10).unwrap();
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    assert_eq!(remaining[0]["path"], json!("/fresh"));
    let ops = store.list_memory_operations(10).unwrap();
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["operation"], json!("SEARCH"));
}

/// A database written before the reason column existed must gain it on open, not fail.
#[tokio::test]
async fn an_old_database_gains_the_reason_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem0.db");
    {
        // The pre-change shape of memory_operations.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memory_operations (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 operation TEXT NOT NULL,
                 detail TEXT NOT NULL DEFAULT '',
                 result_count INTEGER,
                 status_code INTEGER NOT NULL,
                 latency_ms REAL NOT NULL,
                 created_at TEXT NOT NULL
             );",
        )
        .unwrap();
    }
    let store = mem0_desktop_rs::store::StateStore::open(&path).unwrap();
    store
        .log_memory_operation("ADD", "user:alice", Some(0), 200, 1.0, Some("no_memories"))
        .expect("the column must exist after the migration");
    let ops = store.list_memory_operations(10).unwrap();
    assert_eq!(ops[0]["reason"], json!("no_memories"));
}
