//! expiration_date over HTTP: import stores it, reads hide expired rows, export keeps them,
//! and invalid dates are a 400 on POST /memories / a per-item failure on /import.

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

fn yesterday() -> String {
    (chrono::Utc::now().date_naive() - chrono::Duration::days(1)).to_string()
}

fn tomorrow() -> String {
    (chrono::Utc::now().date_naive() + chrono::Duration::days(1)).to_string()
}

fn export_file() -> Value {
    json!({
        "memories": [
            {"id": "exp-1", "memory": "expired fact", "user_id": "alice", "expiration_date": yesterday()},
            {"id": "fut-1", "memory": "future fact", "user_id": "alice", "expiration_date": tomorrow()},
            {"id": "nul-1", "memory": "timeless fact", "user_id": "alice", "expiration_date": null}
        ]
    })
}

#[tokio::test]
async fn expired_memory_hidden_from_list_and_search_but_exported() {
    let a = app();
    configure(&a.router).await;
    let (status, body) = call(&a.router, "POST", "/import", Some(export_file())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], json!(3), "{body}");

    // list hides the expired row
    let (_, listed) = call(&a.router, "GET", "/memories?top_k=100", None).await;
    let ids: Vec<&str> = listed["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.contains(&"exp-1"),
        "expired id must be hidden: {ids:?}"
    );
    assert!(ids.contains(&"fut-1") && ids.contains(&"nul-1"), "{ids:?}");

    // search hides it too
    let (_, found) = call(
        &a.router,
        "POST",
        "/search",
        Some(json!({"query": "fact", "filters": {"user_id": "alice"}, "top_k": 10})),
    )
    .await;
    let found_ids: Vec<&str> = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(!found_ids.contains(&"exp-1"), "{found_ids:?}");
    // Guard against a vacuous pass: the live rows must actually be searchable.
    assert!(
        found_ids.contains(&"fut-1") && found_ids.contains(&"nul-1"),
        "{found_ids:?}"
    );

    // export keeps everything, with the expiration_date field intact
    let (_, exported) = call(&a.router, "GET", "/export?format=json", None).await;
    let rows = exported["memories"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        3,
        "export must include expired rows: {exported}"
    );
    let exp = rows.iter().find(|r| r["id"] == json!("exp-1")).unwrap();
    assert_eq!(exp["expiration_date"], json!(yesterday()), "{exp}");
    let nul = rows.iter().find(|r| r["id"] == json!("nul-1")).unwrap();
    assert_eq!(nul["expiration_date"], Value::Null, "{nul}");
}

#[tokio::test]
async fn post_memories_rejects_invalid_expiration_date_with_400() {
    let a = app();
    configure(&a.router).await;
    let (status, body) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "hello"}],
            "user_id": "alice",
            "infer": false,
            "expiration_date": "31/12/2026"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("YYYY-MM-DD"),
        "{body}"
    );
}

#[tokio::test]
async fn post_memories_accepts_a_valid_expiration_date() {
    let a = app();
    configure(&a.router).await;
    let (status, body) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "remember this"}],
            "user_id": "alice",
            "infer": false,
            "expiration_date": tomorrow()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, exported) = call(&a.router, "GET", "/export?format=json", None).await;
    assert_eq!(
        exported["memories"][0]["expiration_date"],
        json!(tomorrow()),
        "{exported}"
    );
}

#[tokio::test]
async fn import_counts_an_invalid_expiration_date_as_a_per_item_failure() {
    let a = app();
    configure(&a.router).await;
    let bad = json!({"memories": [
        {"id": "ok-1", "memory": "fine", "user_id": "alice"},
        {"id": "bad-1", "memory": "broken date", "user_id": "alice", "expiration_date": "2026-13-99"}
    ]});
    let (status, body) = call(&a.router, "POST", "/import", Some(bad)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], json!(1), "{body}");
    assert_eq!(body["skipped"], json!(1), "{body}");
    assert!(
        body["failures"][0]["error"]
            .as_str()
            .unwrap()
            .contains("YYYY-MM-DD"),
        "{body}"
    );
}
