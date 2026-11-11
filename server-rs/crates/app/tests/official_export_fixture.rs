//! The migration case, as a 69-row export shaped like the official API's output.
//!
//! The file is synthetic: same 69 rows, same field set and the same distribution of
//! categories, lifecycle states and `expiration_date: null`, but with placeholder ids,
//! `user_id` and memory text. Importing it must preserve every id / created_at /
//! updated_at, accept `expiration_date: null` on all rows, and be idempotent on a second
//! pass.

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
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
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

fn fixture() -> Value {
    let raw = include_str!("fixtures/official_export_69.json");
    serde_json::from_str(raw).unwrap()
}

#[tokio::test]
async fn real_official_export_imports_with_every_id_and_timestamp_intact() {
    let a = app();
    configure(&a.router).await;
    let file = fixture();
    let (status, body) = call(&a.router, "POST", "/import", Some(file.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], json!(69), "{body}");
    assert_eq!(body["skipped"], json!(0), "{body}");

    let (_, listed) = call(&a.router, "GET", "/memories?top_k=1000", None).await;
    let rows = listed["results"].as_array().unwrap();
    assert_eq!(rows.len(), 69, "{listed}");

    let original = file["memories"].as_array().unwrap();
    for orig in original {
        let id = orig["id"].as_str().unwrap();
        let row = rows
            .iter()
            .find(|r| r["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("missing id {id} after import"));
        assert_eq!(row["created_at"], orig["created_at"], "id {id}");
        assert_eq!(row["updated_at"], orig["updated_at"], "id {id}");
        assert_eq!(row["memory"], orig["memory"], "id {id}");
        assert_eq!(row["user_id"], orig["user_id"], "id {id}");
        // The fixture carries expiration_date: null on every row — must be accepted.
        assert_eq!(row["expiration_date"], Value::Null, "id {id}");
        // metadata (e.g. channel=fixture) survives as metadata, not flattened away.
        if let Some(md) = orig.get("metadata").and_then(|m| m.as_object()) {
            for (k, v) in md {
                assert_eq!(&row["metadata"][k], v, "id {id} metadata {k}");
            }
        }
    }
}

#[tokio::test]
async fn importing_the_same_official_export_twice_is_idempotent() {
    let a = app();
    configure(&a.router).await;
    let file = fixture();
    call(&a.router, "POST", "/import", Some(file.clone())).await;
    let (status, body) = call(&a.router, "POST", "/import", Some(file)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, listed) = call(&a.router, "GET", "/memories?top_k=1000", None).await;
    assert_eq!(listed["results"].as_array().unwrap().len(), 69, "{listed}");
}

#[tokio::test]
async fn export_after_import_round_trips_ids_and_timestamps() {
    let a = app();
    configure(&a.router).await;
    let file = fixture();
    call(&a.router, "POST", "/import", Some(file.clone())).await;

    let (_, exported) = call(&a.router, "GET", "/export?format=json", None).await;
    let rows = exported["memories"].as_array().unwrap();
    assert_eq!(rows.len(), 69, "{exported}");
    let original = file["memories"].as_array().unwrap();
    for orig in original {
        let id = orig["id"].as_str().unwrap();
        let row = rows.iter().find(|r| r["id"].as_str() == Some(id)).unwrap();
        assert_eq!(row["created_at"], orig["created_at"], "id {id}");
        assert_eq!(row["updated_at"], orig["updated_at"], "id {id}");
    }
}
