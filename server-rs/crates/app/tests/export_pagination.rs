//! `GET /export` as a walk instead of a capped read.
//!
//! The export used to stop at 1000 rows and report `total` as the number it returned, so a
//! library past the cap exported short with nothing in the response saying so. That made
//! "export → reset → re-import" — the only way to change embedding dimensions — lose data
//! silently. These tests pin the new contract: `total_in_store` is the collection, `truncated`
//! says whether the caller still has rows to fetch, and walking every page covers the
//! collection exactly once.

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
    let bytes = axum::body::to_bytes(res.into_body(), 32 * 1024 * 1024)
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
        "vector_store": {
            "provider": "sqlite-vec",
            "config": {"collection_name": "mem0", "embedding_model_dims": 10}
        },
    });
    let (status, resp) = call(app, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
}

async fn add(app: &axum::Router, text: &str) {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": text}],
            "user_id": "alice",
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn an_empty_store_exports_nothing_and_says_so() {
    let a = app();
    configure(&a.router).await;
    let (status, body) = call(&a.router, "GET", "/export?format=json", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0), "{body}");
    assert_eq!(body["total_in_store"], json!(0), "{body}");
    assert_eq!(body["truncated"], json!(false), "{body}");
    assert_eq!(body["memories"], json!([]), "{body}");
}

/// The historical shape is unchanged for a caller that knows nothing about paging.
#[tokio::test]
async fn a_small_store_exports_in_one_response() {
    let a = app();
    configure(&a.router).await;
    for i in 0..5 {
        add(&a.router, &format!("fact {i}")).await;
    }
    let (status, body) = call(&a.router, "GET", "/export?format=json", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(5), "{body}");
    assert_eq!(body["total_in_store"], json!(5), "{body}");
    assert_eq!(body["truncated"], json!(false), "{body}");
}

/// More rows than the cap: the default response is still 1000 rows, but it now admits it.
#[tokio::test]
async fn past_the_cap_the_default_response_is_marked_truncated() {
    let a = app();
    configure(&a.router).await;
    for i in 0..1200 {
        add(&a.router, &format!("fact {i}")).await;
    }

    let (status, body) = call(&a.router, "GET", "/export?format=json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"],
        json!(1000),
        "the old cap still applies: {body}"
    );
    assert_eq!(body["total_in_store"], json!(1200), "{body}");
    assert_eq!(
        body["truncated"],
        json!(true),
        "a capped response must say there is more: {body}"
    );

    // The rest is reachable, and the last page says it is the end.
    let (status, rest) = call(&a.router, "GET", "/export?format=json&offset=1000", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rest["total"], json!(200), "{rest}");
    assert_eq!(rest["total_in_store"], json!(1200), "{rest}");
    assert_eq!(rest["truncated"], json!(false), "{rest}");
}

/// Walking every page must cover the collection exactly once — this is what makes the
/// destructive migration path safe.
#[tokio::test]
async fn walking_every_page_covers_the_collection_exactly_once() {
    let a = app();
    configure(&a.router).await;
    for i in 0..250 {
        add(&a.router, &format!("fact {i}")).await;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut offset = 0;
    loop {
        let uri = format!("/export?format=json&offset={offset}&limit=100");
        let (status, body) = call(&a.router, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total_in_store"], json!(250), "{body}");
        let rows = body["memories"].as_array().expect("memories array");
        if rows.is_empty() {
            assert_eq!(body["truncated"], json!(false), "{body}");
            break;
        }
        for row in rows {
            let text = row["memory"].as_str().unwrap_or_default().to_string();
            seen.push(text);
        }
        offset += rows.len();
        if body["truncated"] == json!(false) {
            break;
        }
    }
    assert_eq!(seen.len(), 250, "no row dropped");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 250, "no row exported twice");
}

/// A limit past the cap is clamped rather than honoured: one response cannot be unbounded,
/// and `truncated` tells the caller to keep walking.
#[tokio::test]
async fn a_limit_above_the_cap_is_clamped_and_reported_honestly() {
    let a = app();
    configure(&a.router).await;
    for i in 0..3 {
        add(&a.router, &format!("fact {i}")).await;
    }
    let (status, body) = call(&a.router, "GET", "/export?format=json&limit=999999", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(3), "{body}");
    assert_eq!(body["truncated"], json!(false), "{body}");

    // limit=0 is a legal, empty page — not a silent fallback to the default.
    let (status, body) = call(&a.router, "GET", "/export?format=json&limit=0", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0), "{body}");
    assert_eq!(body["total_in_store"], json!(3), "{body}");
    assert_eq!(body["truncated"], json!(true), "{body}");
}

/// An offset past the end is an empty page with the real total, so a client that overshoots
/// terminates instead of looping forever.
#[tokio::test]
async fn an_offset_past_the_end_terminates() {
    let a = app();
    configure(&a.router).await;
    add(&a.router, "only one").await;
    let (status, body) = call(&a.router, "GET", "/export?format=json&offset=50", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0), "{body}");
    assert_eq!(body["total_in_store"], json!(1), "{body}");
    assert_eq!(body["truncated"], json!(false), "{body}");
}
