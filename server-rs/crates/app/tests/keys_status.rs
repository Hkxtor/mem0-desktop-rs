//! Key pools over the real HTTP surface: rotation, cooldowns, retirement, and the status line.
//!
//! Every test drives a local mock upstream instead of a provider, because the behaviour under
//! test is *which key a request goes out with and what happens when that key is refused*. A mock
//! can answer `429` with a `Retry-After` on demand; a real provider cannot be asked to. The
//! assertions are therefore on the wire (the `Authorization` header the mock saw, in order) and
//! on the observable result (the request still succeeded, or failed with the pool's own answer).

// Holding a std mutex across await is wrong in production and exactly right here: the guard's
// only job is to stop two tests from configuring the process-wide pool registry at the same time.
#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// One scripted answer: the key it applies to, the status to return, and a `Retry-After`.
type Rule = (String, u16, Option<u64>);

/// What the mock should answer for a given bearer token.
#[derive(Clone, Default)]
struct Upstream {
    /// A key with no rule here gets 200 with a fixed extraction.
    script: Arc<Mutex<Vec<Rule>>>,
    /// Every `Authorization` value seen, in order.
    seen: Arc<Mutex<Vec<String>>>,
}

async fn chat(
    State(up): State<Upstream>,
    headers: HeaderMap,
    _body: Json<Value>,
) -> (StatusCode, HeaderMap, String) {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    up.seen.lock().unwrap().push(auth.clone());

    let script = up.script.lock().unwrap().clone();
    let entry = script.iter().find(|(key, _, _)| auth.ends_with(key));
    let (status, retry_after) = entry.map(|(_, s, r)| (*s, *r)).unwrap_or((200, None));

    let mut out = HeaderMap::new();
    if let Some(secs) = retry_after {
        out.insert("retry-after", secs.to_string().parse().unwrap());
    }
    if status == 200 {
        // A memory the extractor will accept, so a successful attempt is observably successful.
        let body = json!({
            "choices": [{"message": {"role": "assistant", "content": "{\"memory\":[{\"id\":\"0\",\"text\":\"I write Rust\"}]}"}}]
        });
        return (StatusCode::OK, out, body.to_string());
    }
    let body = json!({"error": {"message": format!("upstream says {status}")}});
    (StatusCode::from_u16(status).unwrap(), out, body.to_string())
}

/// The embedder's path: same script, but a successful answer is a vector rather than a chat
/// completion. Embedding failures are *not* swallowed by the pipeline, so this is the path where
/// an exhausted pool becomes an HTTP status the caller sees.
async fn embed(
    State(up): State<Upstream>,
    headers: HeaderMap,
    _body: Json<Value>,
) -> (StatusCode, HeaderMap, String) {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    up.seen.lock().unwrap().push(auth.clone());
    let script = up.script.lock().unwrap().clone();
    let (status, retry_after) = script
        .iter()
        .find(|(key, _, _)| auth.ends_with(key))
        .map(|(_, s, r)| (*s, *r))
        .unwrap_or((200, None));
    let mut out = HeaderMap::new();
    if let Some(secs) = retry_after {
        out.insert("retry-after", secs.to_string().parse().unwrap());
    }
    if status == 200 {
        return (
            StatusCode::OK,
            out,
            json!({"data": [{"embedding": vec![0.1_f32; 10]}]}).to_string(),
        );
    }
    (
        StatusCode::from_u16(status).unwrap(),
        out,
        json!({"error": {"message": format!("upstream says {status}")}}).to_string(),
    )
}

async fn start(script: Vec<Rule>) -> (Upstream, String) {
    let up = Upstream {
        script: Arc::new(Mutex::new(script)),
        seen: Arc::default(),
    };
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/embeddings", post(embed))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (up, format!("http://{addr}"))
}

/// The pool registry is process-wide (it mirrors the running runtime), so tests that configure
/// it must not interleave: one test's three keys would otherwise be the next test's.
fn gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    // The pool registry mirrors the running runtime, which is process-wide: a test that skipped
    // this would read the previous test's keys from `/keys/status`.
    mem0_desktop_rs::pooled::reset();
    let dir = tempfile::tempdir().unwrap();
    let router = mem0_desktop_rs::app_for_test(dir.path().to_path_buf());
    App { _dir: dir, router }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn config(base_url: &str, keys: &[&str]) -> Value {
    json!({
        "version": "v1.1",
        "llm": {
            "provider": "openai",
            "config": {
                "model": "gpt-4o",
                "openai_base_url": base_url,
                "api_keys": keys,
                "api_key": keys.first().copied().unwrap_or_default(),
            }
        },
        "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
        "vector_store": {
            "provider": "sqlite-vec",
            "config": {"collection_name": "mem0", "embedding_model_dims": 10}
        },
    })
}

/// A write that does not ask the model to extract anything, so only the embedder is exercised.
async fn raw_add(router: &Router) -> (StatusCode, Value) {
    call(
        router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I write Rust"}],
            "user_id": "u",
            "infer": false
        })),
    )
    .await
}

async fn add(router: &Router) -> (StatusCode, Value) {
    call(
        router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "I write Rust"}],
            "user_id": "u",
            "infer": true
        })),
    )
    .await
}

async fn status(router: &Router) -> Value {
    let (code, body) = call(router, "GET", "/keys/status", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn requests_rotate_across_the_keys() {
    let _gate = gate();
    let (up, base) = start(vec![]).await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&base, &["sk-a", "sk-b", "sk-c"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    for _ in 0..3 {
        let (code, body) = add(&a.router).await;
        assert_eq!(code, StatusCode::OK, "{body}");
    }
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        ["Bearer sk-a", "Bearer sk-b", "Bearer sk-c"],
        "each request spends the next key"
    );

    let st = status(&a.router).await;
    assert_eq!(st["llm"]["keys"], json!(3), "{st}");
    assert_eq!(st["llm"]["cooling"], json!(0), "{st}");
    assert_eq!(st["llm"]["revoked"], json!(0), "{st}");
}

#[tokio::test]
async fn a_rate_limited_key_is_parked_and_the_request_still_succeeds() {
    let _gate = gate();
    // Key one is out of quota for 30 s; the memory still gets written, on key two.
    let (up, base) = start(vec![
        ("sk-a".into(), 429, Some(30)),
        ("sk-b".into(), 200, None),
        ("sk-c".into(), 200, None),
    ])
    .await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&base, &["sk-a", "sk-b", "sk-c"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, body) = add(&a.router).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 1, "{body}");

    // Two attempts within one request: the refused key, then the next one.
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen, ["Bearer sk-a", "Bearer sk-b"], "{seen:?}");

    let st = status(&a.router).await;
    assert_eq!(st["llm"]["cooling"], json!(1), "{st}");
    let wait = st["llm"]["next_available_in"].as_u64().unwrap();
    assert!(
        (1..=30).contains(&wait),
        "the parked key's remaining cooldown is reported: {st}"
    );

    // The parked key is skipped for the next request rather than retried.
    up.seen.lock().unwrap().clear();
    let (code, _) = add(&a.router).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        up.seen.lock().unwrap().clone(),
        ["Bearer sk-c"],
        "a cooling key is not tried again"
    );
}

#[tokio::test]
async fn a_rejected_key_is_retired_for_the_process() {
    let _gate = gate();
    let (up, base) = start(vec![
        ("sk-a".into(), 401, None),
        ("sk-b".into(), 200, None),
        ("sk-c".into(), 200, None),
    ])
    .await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&base, &["sk-a", "sk-b", "sk-c"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, body) = add(&a.router).await;
    assert_eq!(
        code,
        StatusCode::OK,
        "a rejected key must not fail the write: {body}"
    );
    assert_eq!(
        up.seen.lock().unwrap().clone(),
        ["Bearer sk-a", "Bearer sk-b"]
    );

    let st = status(&a.router).await;
    assert_eq!(st["llm"]["revoked"], json!(1), "{st}");

    // Waiting does not bring it back — unlike a rate limit.
    up.seen.lock().unwrap().clear();
    let (code, _) = add(&a.router).await;
    assert_eq!(code, StatusCode::OK);
    let seen = up.seen.lock().unwrap().clone();
    assert!(
        !seen.iter().any(|a| a == "Bearer sk-a"),
        "a revoked key is never spent again: {seen:?}"
    );
}

/// An embedder section with its own keys, so an exhausted pool surfaces as a status code.
fn embedder_config(base_url: &str, keys: &[&str]) -> Value {
    let mut cfg = config(base_url, &[]);
    cfg["llm"] = json!({"provider": "mock", "config": {}});
    cfg["embedder"] = json!({
        "provider": "openai",
        "config": {
            "model": "text-embedding-3-small",
            "openai_base_url": base_url,
            "api_keys": keys,
            "api_key": keys.first().copied().unwrap_or_default(),
        }
    });
    cfg
}

#[tokio::test]
async fn an_upstream_error_does_not_rotate() {
    let _gate = gate();
    // A 500 is the provider being broken: every key would fail the same way, so rotating only
    // turns one failure into three upstream calls.
    let (up, base) = start(vec![
        ("sk-a".into(), 500, None),
        ("sk-b".into(), 500, None),
        ("sk-c".into(), 500, None),
    ])
    .await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(embedder_config(&base, &["sk-a", "sk-b", "sk-c"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, body) = raw_add(&a.router).await;
    assert!(code.is_server_error(), "{code} {body}");
    assert_eq!(
        up.seen.lock().unwrap().len(),
        1,
        "exactly one attempt, no rotation"
    );
    let st = status(&a.router).await;
    assert_eq!(st["embedder"]["cooling"], json!(0), "{st}");
    assert_eq!(st["embedder"]["revoked"], json!(0), "{st}");
}

#[tokio::test]
async fn every_key_rate_limited_answers_503_with_the_wait() {
    let _gate = gate();
    let (up, base) = start(vec![
        ("sk-a".into(), 429, Some(42)),
        ("sk-b".into(), 429, Some(42)),
    ])
    .await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(embedder_config(&base, &["sk-a", "sk-b"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, body) = raw_add(&a.router).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("rate limited"), "{detail}");
    // The countdown has already lost a fraction of a second by the time it is rendered.
    assert!(
        detail.contains("41s") || detail.contains("42s"),
        "the wait has to be actionable: {detail}"
    );
    // Both keys were tried once, and the message never leaks a key.
    assert_eq!(up.seen.lock().unwrap().len(), 2);
    assert!(!detail.contains("sk-"), "{detail}");

    let st = status(&a.router).await;
    assert_eq!(st["embedder"]["cooling"], json!(2), "{st}");
    assert!(
        (41..=42).contains(&st["embedder"]["next_available_in"].as_u64().unwrap()),
        "{st}"
    );
}

#[tokio::test]
async fn a_section_without_keys_inherits_the_llm_pool() {
    let _gate = gate();
    let (_up, base) = start(vec![]).await;
    let a = app();
    let mut cfg = config(&base, &["sk-a", "sk-b"]);
    // Same provider, no keys of its own: the embedder spends the LLM's.
    cfg["embedder"] = json!({
        "provider": "openai",
        "config": {"model": "text-embedding-3-small", "openai_base_url": base}
    });
    let (code, body) = call(&a.router, "POST", "/configure", Some(cfg)).await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let st = status(&a.router).await;
    assert_eq!(st["embedder"]["keys"], json!(2), "{st}");
    assert_eq!(st["embedder"]["inherited"], json!(true), "{st}");
    assert_eq!(st["llm"]["inherited"], json!(false), "{st}");
    // Nothing was written into the stored config: inheritance is a runtime rule.
    let (_, stored) = call(&a.router, "GET", "/configure", None).await;
    assert!(
        stored["embedder"]["config"].get("api_keys").is_none(),
        "{stored}"
    );
}

#[tokio::test]
async fn an_over_long_list_is_rejected_before_anything_is_built() {
    let _gate = gate();
    let (_up, base) = start(vec![]).await;
    let a = app();
    let keys: Vec<String> = (0..25).map(|i| format!("sk-{i}")).collect();
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let (code, body) = call(&a.router, "POST", "/configure", Some(config(&base, &refs))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("at most"), "{detail}");
    // And the previous (unconfigured) state is untouched: no half-applied config.
    let (code, _) = call(&a.router, "GET", "/memories?top_k=5", None).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn the_configure_response_redacts_every_key_but_keeps_the_count() {
    let _gate = gate();
    let (_up, base) = start(vec![]).await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&base, &["sk-a", "sk-b", "sk-c"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, stored) = call(&a.router, "GET", "/configure", None).await;
    assert_eq!(code, StatusCode::OK);
    let keys = stored["llm"]["config"]["api_keys"]
        .as_array()
        .unwrap_or_else(|| panic!("{stored}"));
    // The count is not a secret (it is what the status line shows); the values are.
    assert_eq!(keys.len(), 3, "{stored}");
    for key in keys {
        assert_eq!(key, &json!("[redacted]"), "{stored}");
    }
    assert!(!stored.to_string().contains("sk-a"), "{stored}");
}

#[tokio::test]
async fn a_single_key_still_behaves_like_a_single_key() {
    let _gate = gate();
    // The pool must not park the only key it has: a 429 has to come back as the provider's own
    // error, exactly as it did before pools existed.
    let (up, base) = start(vec![("sk-only".into(), 429, Some(30))]).await;
    let a = app();
    let (code, body) = call(
        &a.router,
        "POST",
        "/configure",
        Some(config(&base, &["sk-only"])),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    // Extraction failures are deliberately swallowed by the pipeline (the reason lands in the
    // operation record), so the observable result here is "nothing was stored" — and exactly one
    // upstream attempt, not a rotation to a key that does not exist.
    let (code, body) = add(&a.router).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["results"], json!([]), "{body}");
    assert_eq!(up.seen.lock().unwrap().len(), 1);

    // And it is not locked out afterwards: the next request tries again.
    up.seen.lock().unwrap().clear();
    let (code, _) = add(&a.router).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        up.seen.lock().unwrap().len(),
        1,
        "a retry reaches the provider instead of being blocked for a minute"
    );
    let st = status(&a.router).await;
    assert_eq!(st["llm"]["keys"], json!(1), "{st}");
    assert_eq!(st["llm"]["cooling"], json!(0), "{st}");
}
