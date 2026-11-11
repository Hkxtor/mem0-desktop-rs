//! `POST /import` fidelity: an official-API export keeps its id and timestamps.
//!
//! The importer is the migration path from the hosted API, where every memory already has a
//! stable id and created/updated timestamps. Regenerating them on import breaks external
//! references and history, so these tests pin: fields survive, re-import is idempotent, and a
//! changed payload under the same id overwrites (matching the Python version's
//! INSERT OR REPLACE semantics).

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

async fn configure(app: &axum::Router) {
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    });
    let (status, resp) = call(app, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
}

/// Two items shaped like the official export, with fixed ids and timestamps.
fn official_export() -> Value {
    json!({
        "memories": [
            {
                "id": "aac45e70-56d5-4d79-9d0b-df736d7ef129",
                "memory": "User asked the assistant to check how many memories are stored",
                "user_id": "fixture-user",
                "metadata": {"channel": "weixin"},
                "created_at": "2026-11-06T18:08:56-08:00",
                "updated_at": "2026-11-06T18:09:20.014941-08:00"
            },
            {
                "id": "b5dcb70c-5144-47ce-9824-ded4edcae2eb",
                "memory": "User lives in Berlin",
                "user_id": "alice",
                "metadata": {},
                "created_at": "2026-11-05T10:00:00+00:00",
                "updated_at": "2026-11-05T10:00:00+00:00"
            }
        ]
    })
}

#[tokio::test]
async fn import_preserves_id_and_timestamps() {
    let a = app();
    configure(&a.router).await;
    let (status, body) = call(&a.router, "POST", "/import", Some(official_export())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], json!(2), "{body}");

    let (_, listed) = call(&a.router, "GET", "/memories?top_k=100", None).await;
    let rows = listed["results"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{listed}");
    let by_id = |id: &str| rows.iter().find(|r| r["id"] == json!(id)).cloned().unwrap();

    let first = by_id("aac45e70-56d5-4d79-9d0b-df736d7ef129");
    assert_eq!(
        first["created_at"],
        json!("2026-11-06T18:08:56-08:00"),
        "{first}"
    );
    assert_eq!(
        first["updated_at"],
        json!("2026-11-06T18:09:20.014941-08:00"),
        "{first}"
    );
    // metadata survives the trip too.
    assert_eq!(first["metadata"]["channel"], json!("weixin"), "{first}");
}

#[tokio::test]
async fn reimporting_the_same_file_is_idempotent() {
    let a = app();
    configure(&a.router).await;
    call(&a.router, "POST", "/import", Some(official_export())).await;
    let (status, body) = call(&a.router, "POST", "/import", Some(official_export())).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, listed) = call(&a.router, "GET", "/memories?top_k=100", None).await;
    assert_eq!(listed["results"].as_array().unwrap().len(), 2, "{listed}");
}

#[tokio::test]
async fn same_id_with_new_content_overwrites() {
    let a = app();
    configure(&a.router).await;
    call(&a.router, "POST", "/import", Some(official_export())).await;

    let mut changed = official_export();
    changed["memories"][0]["memory"] = json!("User asked something else entirely");
    let (status, _) = call(&a.router, "POST", "/import", Some(changed)).await;
    assert_eq!(status, StatusCode::OK);

    let (_, listed) = call(&a.router, "GET", "/memories?top_k=100", None).await;
    let rows = listed["results"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "overwrite must not duplicate: {listed}");
    let row = rows
        .iter()
        .find(|r| r["id"] == json!("aac45e70-56d5-4d79-9d0b-df736d7ef129"))
        .unwrap();
    assert_eq!(
        row["memory"],
        json!("User asked something else entirely"),
        "{row}"
    );
    // The original timestamps are kept, only the content changed.
    assert_eq!(
        row["created_at"],
        json!("2026-11-06T18:08:56-08:00"),
        "{row}"
    );
}

#[tokio::test]
async fn import_still_rejects_empty_memory_or_user_id() {
    let a = app();
    configure(&a.router).await;
    let bad = json!({"memories": [
        {"id": "x", "memory": "", "user_id": "alice"},
        {"id": "y", "memory": "ok", "user_id": ""},
        {"id": "z", "memory": "fine", "user_id": "alice"}
    ]});
    let (status, body) = call(&a.router, "POST", "/import", Some(bad)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], json!(1), "{body}");
    assert_eq!(body["skipped"], json!(2), "{body}");
}

// --- R6 remainder: import is admin-gated and lands in the operations log ---

#[tokio::test]
async fn import_is_recorded_as_an_import_operation() {
    let a = app();
    configure(&a.router).await;
    call(&a.router, "POST", "/import", Some(official_export())).await;

    let (_, rows) = call(&a.router, "GET", "/requests", None).await;
    let ops = rows.as_array().unwrap();
    let import_rows: Vec<&Value> = ops
        .iter()
        .filter(|r| r["operation"] == json!("IMPORT"))
        .collect();
    assert_eq!(import_rows.len(), 1, "{rows}");
    assert!(
        import_rows[0]["detail"]
            .as_str()
            .unwrap()
            .contains("2 memories"),
        "{rows}"
    );
    assert_eq!(import_rows[0]["results"], json!(2), "{rows}");
    assert_eq!(import_rows[0]["status_code"], json!(200), "{rows}");
}
