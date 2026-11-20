//! Regression: a memory that was stored, reported by `/requests`, and missing from the list.
//!
//! Field report: add a memory, the Requests row says it stored one, and the Memories page does
//! not show it. The memory was there the whole time — `GET /memories/{id}` returned it and
//! search found it. The listing was the problem: `SELECT ... LIMIT 1000` over a vec0 table with
//! no `ORDER BY`, so the dashboard received 1000 arbitrary rows out of 4825 and nothing said
//! the other 3825 existed. A memory added a minute ago simply was not in that slice.
//!
//! Two invariants are pinned here: a listing comes back newest-first (so a new memory is on the
//! first page), and `offset` walks the whole collection without repeating or dropping a row.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

async fn seeded_app(count: usize) -> (tempfile::TempDir, axum::Router) {
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
    let app = mem0_desktop_rs::app_for_test_with_memory(dir.path().to_path_buf(), memory);
    for i in 0..count {
        let (status, body) = call(
            &app,
            "POST",
            "/memories",
            Some(serde_json::json!({
                "messages": [{"role": "user", "content": format!("fact number {i}")}],
                "user_id": "alice",
                "infer": false,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    (dir, app)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ids(page: &Value) -> Vec<String> {
    page["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|m| m["id"].as_str().expect("id").to_string())
        .collect()
}

#[tokio::test]
async fn a_newly_added_memory_is_on_the_first_page() {
    let (_dir, app) = seeded_app(30).await;
    let (status, added) = call(
        &app,
        "POST",
        "/memories",
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "the one I just added"}],
            "user_id": "alice",
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    let new_id = added["results"][0]["id"].as_str().expect("id").to_string();

    // What the dashboard asks for on load: the first page.
    let (status, page) = call(&app, "GET", "/memories?top_k=20", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], serde_json::json!(31), "{page}");
    assert!(
        ids(&page).contains(&new_id),
        "the memory just added must be on the first page: {page}"
    );
    assert_eq!(
        page["results"][0]["id"],
        serde_json::json!(new_id),
        "and it must be first: the listing is newest-first"
    );
}

#[tokio::test]
async fn walking_with_offset_reaches_every_memory_once() {
    let (_dir, app) = seeded_app(25).await;

    let mut seen: Vec<String> = Vec::new();
    let mut offset = 0;
    loop {
        let (status, page) = call(
            &app,
            "GET",
            &format!("/memories?top_k=10&offset={offset}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["total"], serde_json::json!(25), "total is the library");
        let page_ids = ids(&page);
        if page_ids.is_empty() {
            break;
        }
        offset += page_ids.len();
        seen.extend(page_ids);
    }

    assert_eq!(seen.len(), 25, "no row dropped: {seen:?}");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 25, "no row repeated: {seen:?}");

    // The listing order is chronological, so the walk starts at the newest.
    let (_, whole) = call(&app, "GET", "/memories?top_k=50", None).await;
    assert_eq!(ids(&whole), seen, "paged and single-shot agree");
}

#[tokio::test]
async fn an_offset_past_the_end_is_empty_and_still_reports_the_total() {
    let (_dir, app) = seeded_app(3).await;
    let (status, page) = call(&app, "GET", "/memories?top_k=10&offset=50", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(ids(&page).is_empty(), "{page}");
    assert_eq!(page["total"], serde_json::json!(3), "{page}");
}

#[tokio::test]
async fn an_expired_memory_stays_out_of_the_pages() {
    let (_dir, app) = seeded_app(2).await;
    let (status, expired) = call(
        &app,
        "POST",
        "/memories",
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "old news"}],
            "user_id": "alice",
            "infer": false,
            "expiration_date": "2020-01-01",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{expired}");
    let expired_id = expired["results"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();

    let (_, page) = call(&app, "GET", "/memories?top_k=50", None).await;
    assert!(
        !ids(&page).contains(&expired_id),
        "an expired memory is hidden, not paged in: {page}"
    );
}
