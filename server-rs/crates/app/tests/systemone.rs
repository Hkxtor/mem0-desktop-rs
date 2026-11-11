//! `/systemone/*` over HTTP: preset listing, classification via a configured
//! systemone model (mock provider with a canned `response`), and the disabled state.

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

async fn configure(app: &axum::Router, systemone: Value) {
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": systemone,
    });
    let (status, resp) = call(app, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
}

#[tokio::test]
async fn classify_returns_preset_subset_and_filters_unknown() {
    let a = app();
    configure(
        &a.router,
        json!({"provider": "mock", "config": {"response": "[\"technology\",\"food\",\"bogus\",\"technology\"]"}}),
    )
    .await;
    let (status, body) = call(
        &a.router,
        "POST",
        "/systemone/classify",
        Some(json!({"text": "I built a RAG pipeline at work"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["categories"], json!(["technology", "food"]), "{body}");
}

#[tokio::test]
async fn classify_rejects_empty_text_with_400() {
    let a = app();
    configure(
        &a.router,
        json!({"provider": "mock", "config": {"response": "[]"}}),
    )
    .await;
    let (status, body) = call(
        &a.router,
        "POST",
        "/systemone/classify",
        Some(json!({"text": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn classify_is_503_when_systemone_not_configured() {
    let a = app();
    // configure WITHOUT a systemone section
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
    });
    let (status, _) = call(&a.router, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &a.router,
        "POST",
        "/systemone/classify",
        Some(json!({"text": "anything"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("systemone_not_configured"),
        "{body}"
    );

    let (status, body) = call(&a.router, "GET", "/systemone/categories", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], json!(false), "{body}");
    assert_eq!(body["categories"].as_array().unwrap().len(), 15, "{body}");
}

#[tokio::test]
async fn categories_endpoint_reports_configured_and_full_preset() {
    let a = app();
    configure(
        &a.router,
        json!({"provider": "mock", "config": {"response": "[]"}}),
    )
    .await;
    let (status, body) = call(&a.router, "GET", "/systemone/categories", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], json!(true), "{body}");
    let cats = body["categories"].as_array().unwrap();
    assert_eq!(cats.len(), 15);
    assert!(cats.iter().any(|c| c == "technology"));
    assert!(cats.iter().any(|c| c == "misc"));
}

#[tokio::test]
async fn configure_rejects_unknown_systemone_provider() {
    let a = app();
    let body = json!({
        "version": "v1.1",
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "systemone": {"provider": "bamboonode", "config": {}},
    });
    let (status, resp) = call(&a.router, "POST", "/configure", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    assert!(
        resp["detail"].as_str().unwrap().contains("systemone"),
        "{resp}"
    );
}

#[tokio::test]
async fn provider_switch_without_new_key_drops_stale_systemone_key() {
    // The SECTIONS cleanup rules now cover systemone: switching provider without
    // supplying a new api_key must not leave the old provider's key behind.
    let a = app();
    configure(
        &a.router,
        json!({"provider": "openai", "config": {"model": "gpt-5-mini", "api_key": "sk-old"}}),
    )
    .await;
    let (status, resp) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({"systemone": {"provider": "anthropic", "config": {"model": "claude-haiku-4-5"}}})),
    )
    .await;
    // anthropic without a key may or may not build (no key validation offline), but
    // the point under test is the cleanup rule: switching provider without a new key
    // must drop the old key. Switch straight to mock, which always builds.
    let (status, resp) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({"systemone": {"provider": "mock", "config": {"response": "[]"}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let (_, cfg) = call(&a.router, "GET", "/configure", None).await;
    let sys = &cfg["systemone"];
    assert_eq!(sys["provider"], json!("mock"), "{cfg}");
    // GET /configure is redacted; check the raw file instead
    let raw = std::fs::read_to_string(a._dir.path().join("config.json")).unwrap();
    assert!(!raw.contains("sk-old"), "stale key must be dropped: {raw}");
}

#[tokio::test]
async fn empty_provider_turns_categorization_off() {
    // The Configuration page sends {provider: ""} when the operator selects "Off".
    // That must be accepted (not a 400 for an unsupported provider) and must stop
    // classification, leaving new memories untagged.
    let a = app();
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({
            "version": "v1.1",
            "llm": {"provider": "mock", "config": {}},
            "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
            "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
            "systemone": {"provider": "mock", "config": {"response": "[\"technology\"]"}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // on: a new memory gets tagged
    let (_, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({"messages": [{"role": "user", "content": "a rust fact"}], "user_id": "u", "infer": false})),
    )
    .await;
    assert_eq!(
        added["results"][0]["categories"],
        json!(["technology"]),
        "{added}"
    );

    // off
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({
            "version": "v1.1",
            "llm": {"provider": "mock", "config": {}},
            "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
            "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
            "systemone": {"provider": "", "config": {}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({"messages": [{"role": "user", "content": "another fact"}], "user_id": "u", "infer": false})),
    )
    .await;
    assert_eq!(added["results"][0]["categories"], json!([]), "{added}");

    // and the endpoint reports it as off
    let (_, cats) = call(&a.router, "GET", "/systemone/categories", None).await;
    assert_eq!(cats["configured"], json!(false), "{cats}");
}

#[tokio::test]
async fn unreachable_provider_is_recorded_not_swallowed() {
    // The exact shape of the reported failure: a systemone section pointing at a base URL
    // that has no /chat/completions. Add still succeeds (best-effort), so without
    // last_error an operator cannot tell this apart from "no category applies".
    let a = app();
    let (status, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({
            "version": "v1.1",
            "llm": {"provider": "mock", "config": {}},
            "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
            "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
            "systemone": {
                "provider": "openai",
                "config": {
                    "model": "probe-model",
                    // nothing listens here, so the request fails immediately
                    "api_key": "sk-test",
                    "openai_base_url": "http://127.0.0.1:1/v1/systemone",
                },
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // add still succeeds, untagged
    let (status, added) = call(
        &a.router,
        "POST",
        "/memories",
        Some(json!({"messages": [{"role": "user", "content": "a rust fact"}], "user_id": "u", "infer": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["results"][0]["categories"], json!([]), "{added}");

    // but the failure is visible
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    let err = counts["last_error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !err.is_empty(),
        "last_error should record the failure: {counts}"
    );
    assert!(err.contains("LLM"), "{err}");
    assert_eq!(counts["provider"], json!("openai"), "{counts}");
    assert_eq!(counts["model"], json!("probe-model"), "{counts}");

    // /systemone/test reports it verbatim and stays 200 so the page can render it
    let (status, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(false), "{tested}");
    assert_eq!(tested["configured"], json!(true), "{tested}");
    assert!(
        !tested["error"].as_str().unwrap_or_default().is_empty(),
        "{tested}"
    );

    // fixing the configuration clears the stale error
    let (status, _) = call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({
            "version": "v1.1",
            "llm": {"provider": "mock", "config": {}},
            "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
            "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
            "systemone": {"provider": "mock", "config": {"response": "[\"technology\"]"}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, counts) = call(&a.router, "GET", "/systemone/counts", None).await;
    assert!(counts["last_error"].is_null(), "{counts}");

    let (_, tested) = call(&a.router, "POST", "/systemone/test", Some(json!({}))).await;
    assert_eq!(tested["ok"], json!(true), "{tested}");
    assert_eq!(tested["categories"], json!(["technology"]), "{tested}");
}
