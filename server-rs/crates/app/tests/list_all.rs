//! Regression: memories that exist but never appear.
//!
//! Field report: `POST /memories` returned an ADD result, the dashboard's Memories page
//! stayed empty. The unfiltered listing (`GET /memories?top_k=N`, which is what the page
//! sends when its user filter is blank) and the export endpoint read `vectors.db` with a
//! plain SQLite connection. `mem0` is a vec0 virtual table, so that connection reports
//! `no such module: vec0` and the failure was swallowed into an empty list. The golden
//! tests missed it because they compare against an empty database, where `[]` is right.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

async fn seeded_app() -> (tempfile::TempDir, axum::Router, String) {
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
    let id = {
        use mem0::AddOptions;
        memory
            .add(
                "Alice likes hiking",
                AddOptions {
                    user_id: Some("alice".into()),
                    infer: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("seed add")[0]
            .id
            .clone()
    };
    let app = mem0_desktop_rs::app_for_test_with_memory(dir.path().to_path_buf(), memory);
    (dir, app, id)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn unfiltered_listing_returns_stored_memories() {
    let (_dir, app, id) = seeded_app().await;
    for uri in ["/memories?top_k=100", "/memories", "/v1/memories"] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let results = body["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{uri}: {body}"));
        assert_eq!(
            results.len(),
            1,
            "{uri} must list the stored memory, got {body}"
        );
        assert_eq!(results[0]["id"], serde_json::json!(id), "{uri}");
        assert_eq!(
            results[0]["memory"],
            serde_json::json!("Alice likes hiking"),
            "{uri}"
        );
        // Python's _serialize_memory never reports a similarity for a listing.
        assert!(
            results[0].get("score").is_none(),
            "{uri} must not invent a score: {body}"
        );
    }
}

#[tokio::test]
async fn export_carries_what_import_needs() {
    let (_dir, app, id) = seeded_app().await;
    let (status, body) = get(&app, "/export?format=json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"],
        serde_json::json!(1),
        "export must not be empty: {body}"
    );
    let item = &body["memories"][0];
    assert_eq!(item["id"], serde_json::json!(id));
    // The importer requires `memory`; an export without it cannot be restored.
    assert_eq!(item["memory"], serde_json::json!("Alice likes hiking"));
    assert_eq!(item["user_id"], serde_json::json!("alice"));
}

#[tokio::test]
async fn entities_are_bucketed_from_stored_memories() {
    let (_dir, app, _id) = seeded_app().await;
    let (status, body) = get(&app, "/entities").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body
        .as_array()
        .unwrap_or_else(|| panic!("entities: {body}"));
    assert_eq!(arr.len(), 1, "one user bucket expected: {body}");
    assert_eq!(arr[0]["id"], serde_json::json!("alice"));
    assert_eq!(arr[0]["total_memories"], serde_json::json!(1));
}
