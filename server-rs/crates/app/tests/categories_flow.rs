//! End-to-end categories flow over HTTP with a mock systemone model:
//! add auto-classifies and writes back, list/search items carry top-level categories,
//! filters.categories is AND (all requested categories must be present), export/import
//! round-trip preserves categories, and classify-memory re-tags an existing row.

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

async fn configure_with_systemone(app: &axum::Router, response: &str) {
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": {"provider": "mock", "config": {"response": response}},
    });
    let (status, resp) = call(app, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
}

async fn add_memory(app: &axum::Router, text: &str, user_id: &str) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": text}],
            "user_id": user_id,
            "infer": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn add_auto_classifies_and_list_carries_top_level_categories() {
    let a = app();
    configure_with_systemone(&a.router, r#"["technology"]"#).await;
    let body = add_memory(&a.router, "I built a RAG pipeline", "alice").await;
    let item = &body["results"][0];
    assert_eq!(item["categories"], json!(["technology"]), "{body}");
    let id = item["id"].as_str().unwrap().to_string();

    // the write-back landed in the payload: list shows it too
    let (_, listed) = call(&a.router, "GET", "/memories?user_id=alice&top_k=10", None).await;
    let row = &listed["results"][0];
    assert_eq!(row["id"], json!(id));
    assert_eq!(row["categories"], json!(["technology"]), "{listed}");
    // payload fields survived the read-merge-write
    assert_eq!(row["user_id"], json!("alice"), "{row}");
    assert!(row["created_at"].is_string(), "{row}");
}

#[tokio::test]
async fn add_without_systemone_has_empty_categories_and_no_error() {
    let a = app();
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    });
    let (status, _) = call(&a.router, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK);

    let body = add_memory(&a.router, "plain fact", "bob").await;
    assert_eq!(body["results"][0]["categories"], json!([]), "{body}");
}

#[tokio::test]
async fn categories_filter_is_and_semantics() {
    let a = app();
    // mock returns the SAME categories for every classify call, so import rows with
    // explicit categories instead (import path preserves preset values verbatim).
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    });
    let (status, _) = call(&a.router, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK);

    let file = json!({"memories": [
        {"id": "m-food", "memory": "loves ramen", "user_id": "alice", "categories": ["food"]},
        {"id": "m-both", "memory": "food trip to japan", "user_id": "alice", "categories": ["food", "travel"]},
        {"id": "m-tech", "memory": "likes rust", "user_id": "alice", "categories": ["technology", "bogus-label"]}
    ]});
    let (status, resp) = call(&a.router, "POST", "/import", Some(file)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["imported"], json!(3), "{resp}");

    // AND: both food and travel required -> only m-both
    let (_, res) = call(
        &a.router,
        "GET",
        "/memories?user_id=alice&categories=food,categories=travel&top_k=10",
        None,
    )
    .await;
    // query-string array syntax may vary; use the POST search form instead which takes JSON filters
    let (_, res) = call(
        &a.router,
        "POST",
        "/search",
        Some(json!({"query": "food travel", "filters": {"user_id": "alice", "categories": ["food", "travel"]}, "top_k": 10})),
    )
    .await;
    let ids: Vec<&str> = res["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"m-both"), "{ids:?}");
    assert!(
        !ids.contains(&"m-food"),
        "AND semantics: food-only row must be excluded: {ids:?}"
    );

    // single category matches both food rows
    let (_, res) = call(
        &a.router,
        "POST",
        "/search",
        Some(json!({"query": "food", "filters": {"user_id": "alice", "categories": ["food"]}, "top_k": 10})),
    )
    .await;
    let ids: Vec<&str> = res["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"m-food") && ids.contains(&"m-both"),
        "{ids:?}"
    );

    // unknown label was dropped at import; export keeps only preset values
    let (_, exported) = call(&a.router, "GET", "/export?format=json", None).await;
    let tech = exported["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("m-tech"))
        .unwrap()
        .clone();
    assert_eq!(tech["categories"], json!(["technology"]), "{tech}");
}

#[tokio::test]
async fn classify_memory_rewrites_existing_row_and_404s_unknown_id() {
    let a = app();
    configure_with_systemone(&a.router, r#"["food","travel"]"#).await;
    let body = add_memory(&a.router, "some text", "carol").await;
    let id = body["results"][0]["id"].as_str().unwrap().to_string();

    let (status, resp) = call(
        &a.router,
        "POST",
        "/systemone/classify-memory",
        Some(json!({"memory_id": id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["categories"], json!(["food", "travel"]), "{resp}");

    let (_, listed) = call(&a.router, "GET", "/memories?user_id=carol&top_k=10", None).await;
    assert_eq!(
        listed["results"][0]["categories"],
        json!(["food", "travel"]),
        "{listed}"
    );

    let (status, _) = call(
        &a.router,
        "POST",
        "/systemone/classify-memory",
        Some(json!({"memory_id": "no-such-id"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn export_import_round_trips_categories() {
    let a = app();
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    });
    let (status, _) = call(&a.router, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK);

    let file = json!({"memories": [
        {"id": "x1", "memory": "multi label", "user_id": "dave", "categories": ["professional_details", "technology"]},
        {"id": "x2", "memory": "no label", "user_id": "dave"}
    ]});
    let (status, resp) = call(&a.router, "POST", "/import", Some(file.clone())).await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    let (_, exported) = call(&a.router, "GET", "/export?format=json", None).await;
    let x1 = exported["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("x1"))
        .unwrap()
        .clone();
    assert_eq!(
        x1["categories"],
        json!(["professional_details", "technology"]),
        "{x1}"
    );
    let x2 = exported["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("x2"))
        .unwrap()
        .clone();
    assert_eq!(x2["categories"], json!([]), "{x2}");
}

#[tokio::test]
async fn get_memories_categories_query_param_filters_and_counts_endpoint_aggregates() {
    let a = app();
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    });
    let (status, _) = call(&a.router, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK);

    let file = json!({"memories": [
        {"id": "q1", "memory": "ramen", "user_id": "erin", "categories": ["food"]},
        {"id": "q2", "memory": "trip", "user_id": "erin", "categories": ["food", "travel"]},
        {"id": "q3", "memory": "rust", "user_id": "erin", "categories": ["technology"]}
    ]});
    let (status, resp) = call(&a.router, "POST", "/import", Some(file)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    // scoped + categories (AND)
    let (_, res) = call(
        &a.router,
        "GET",
        "/memories?user_id=erin&categories=food,travel&top_k=10",
        None,
    )
    .await;
    let ids: Vec<&str> = res["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["q2"], "{ids:?}");

    // categories only, NO scope (admin path through list_all_filtered)
    let (_, res) = call(
        &a.router,
        "GET",
        "/memories?categories=food&top_k=100",
        None,
    )
    .await;
    let mut ids: Vec<&str> = res["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["q1", "q2"], "{ids:?}");

    // no categories param -> unchanged behavior
    let (_, res) = call(&a.router, "GET", "/memories?top_k=100", None).await;
    assert_eq!(res["results"].as_array().unwrap().len(), 3, "{res}");

    // counts endpoint
    let (status, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert_eq!(status, StatusCode::OK, "{counts}");
    assert_eq!(counts["configured"], json!(false), "{counts}");
    assert_eq!(counts["counts"]["food"], json!(2), "{counts}");
    assert_eq!(counts["counts"]["travel"], json!(1), "{counts}");
    assert_eq!(counts["counts"]["technology"], json!(1), "{counts}");
    assert!(counts["counts"].get("music").is_none(), "{counts}");
}
