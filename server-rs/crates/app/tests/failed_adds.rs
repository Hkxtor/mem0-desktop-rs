//! The retry queue over the real HTTP surface: what an `add` keeps when it stores nothing, and
//! what a retry does with it.
//!
//! Every test drives a local mock upstream, because the behaviour under test is *what happens
//! after the provider refuses or the embedder drops an entry* — and a real provider cannot be
//! asked to answer `429` and then behave. The mock can be switched mid-test, which is also how
//! a retry is observed: the same stashed input, the upstream fixed, the memory appearing.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The pool registry mirrors process-wide state, so tests that configure it must not interleave.
fn gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A provider that can be broken and repaired while the test runs.
#[derive(Clone)]
struct Upstream {
    /// Chat completions answer `500` while this is set.
    chat_broken: Arc<AtomicBool>,
    /// Embeddings answer `500` while this is set.
    embed_broken: Arc<AtomicBool>,
    /// Embeddings of a text containing this answer `500` (a per-entry refusal).
    poison: Arc<Mutex<Option<String>>>,
    /// The extraction answer to return when the chat call works.
    answer: Arc<Mutex<String>>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            chat_broken: Arc::new(AtomicBool::new(false)),
            embed_broken: Arc::new(AtomicBool::new(false)),
            poison: Arc::new(Mutex::new(None)),
            answer: Arc::new(Mutex::new(
                json!({"memory": [{"id": "0", "text": "I write Rust"}]}).to_string(),
            )),
        }
    }
}

async fn chat(State(up): State<Upstream>, _body: Json<Value>) -> (StatusCode, String) {
    if up.chat_broken.load(Ordering::SeqCst) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": "upstream is down"}}).to_string(),
        );
    }
    let answer = up.answer.lock().unwrap().clone();
    (
        StatusCode::OK,
        json!({"choices": [{"message": {"role": "assistant", "content": answer}}]}).to_string(),
    )
}

async fn embed(State(up): State<Upstream>, Json(body): Json<Value>) -> (StatusCode, String) {
    let poison = up.poison.lock().unwrap().clone();
    if let Some(needle) = poison {
        if body.to_string().contains(&needle) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": "that text is too strange"}}).to_string(),
            );
        }
    }
    if up.embed_broken.load(Ordering::SeqCst) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": "no vectors today"}}).to_string(),
        );
    }
    (
        StatusCode::OK,
        // `index` is required by the client's decoder.
        json!({"data": [{"embedding": vec![0.1_f32; 10], "index": 0}]}).to_string(),
    )
}

async fn start() -> (Upstream, String) {
    let up = Upstream::default();
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

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
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

async fn configure(app: &Router, base_url: &str) {
    let cfg = json!({
        "version": "v1.1",
        "llm": {"provider": "openai", "config": {
            "model": "gpt-4o", "openai_base_url": base_url, "api_key": "k"
        }},
        "embedder": {"provider": "openai", "config": {
            "model": "text-embedding-3-small", "openai_base_url": base_url,
            "api_key": "k", "embedding_dims": 10
        }},
        "vector_store": {"provider": "sqlite-vec", "config": {
            "collection_name": "mem0", "embedding_model_dims": 10
        }},
    });
    let (status, body) = call(app, "POST", "/configure", Some(cfg)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

const MESSAGE: &str = "I moved to Hangzhou and ride to work";

async fn add(app: &Router, content: &str) -> Value {
    let (status, body) = call(
        app,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": content}],
            "user_id": "u",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn queue(app: &Router) -> Value {
    let (status, body) = call(app, "GET", "/memories/failed", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn retry(app: &Router, body: Value) -> Value {
    let (status, out) = call(app, "POST", "/memories/failed/retry", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    out
}

/// The most recent ADD row from the Requests endpoint.
async fn last_operation(app: &Router) -> Value {
    let (status, rows) = call(app, "GET", "/requests?limit=5", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    rows.as_array()
        .and_then(|rows| rows.iter().find(|r| r["operation"] == "ADD").cloned())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_provider_error_keeps_the_input_and_a_retry_stores_it() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    up.chat_broken.store(true, Ordering::SeqCst);

    let body = add(&app.router, MESSAGE).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0, "{body}");

    // The input survived the connection that dropped it.
    let q = queue(&app.router).await;
    assert_eq!(q["total"], 1, "{q}");
    let row = &q["items"][0];
    assert_eq!(row["reason"], "provider_error");
    assert_eq!(row["messages"][0]["content"], MESSAGE);
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["user_id"], "u");
    assert!(
        row["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("upstream is down"),
        "the provider's own words are kept: {row}"
    );

    // The Requests row points at it, which is how the page offers a retry.
    let op = last_operation(&app.router).await;
    assert_eq!(op["reason"], "provider_error");
    assert_eq!(op["failed_add_id"], row["id"]);

    // Fix the upstream and redo the work.
    up.chat_broken.store(false, Ordering::SeqCst);
    let out = retry(&app.router, json!({ "ids": [row["id"]] })).await;
    assert_eq!(out["stored"], 1, "{out}");
    assert_eq!(out["remaining"], 0, "{out}");

    let (_, memories) = call(&app.router, "GET", "/memories?user_id=u", None).await;
    let stored = memories["results"].as_array().unwrap();
    assert!(
        stored.iter().any(|m| m["memory"] == "I write Rust"),
        "{memories}"
    );

    // The messages live on as the memory's Input, so the row is redundant and gone.
    let id = stored[0]["id"].as_str().unwrap();
    let (_, inputs) = call(&app.router, "GET", &format!("/memories/{id}/inputs"), None).await;
    assert_eq!(inputs[0]["content"], MESSAGE, "{inputs}");
    assert_eq!(queue(&app.router).await["total"], 0);
}

#[tokio::test]
async fn the_same_input_failing_twice_is_one_row_with_two_attempts() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    up.chat_broken.store(true, Ordering::SeqCst);

    add(&app.router, MESSAGE).await;
    add(&app.router, MESSAGE).await;

    let q = queue(&app.router).await;
    assert_eq!(
        q["total"], 1,
        "retrying a request is one piece of work: {q}"
    );
    assert_eq!(q["items"][0]["attempts"], 2);
}

#[tokio::test]
async fn a_zero_the_model_asked_for_is_explained_but_not_stashed() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    *up.answer.lock().unwrap() = json!({"memory": []}).to_string();

    let body = add(&app.router, MESSAGE).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0, "{body}");

    // Explained on the Requests page...
    assert_eq!(last_operation(&app.router).await["reason"], "no_memories");
    // ...but nothing was lost, so there is nothing to retry.
    assert_eq!(queue(&app.router).await["total"], 0);
}

#[tokio::test]
async fn an_unparseable_answer_is_stashed_without_entries() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    *up.answer.lock().unwrap() = "I could not decide.".to_string();

    add(&app.router, MESSAGE).await;

    let q = queue(&app.router).await;
    assert_eq!(q["total"], 1, "{q}");
    assert_eq!(q["items"][0]["reason"], "unparsed_answer");
    assert_eq!(q["items"][0]["items"].as_array().unwrap().len(), 0);
    assert_eq!(q["items"][0]["messages"][0]["content"], MESSAGE);
}

#[tokio::test]
async fn an_add_the_pipeline_could_not_finish_keeps_its_input() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    // The embedder is down, so the add throws before storing anything: the client gets a 500
    // and, without a stash, would have to resend a request it no longer has.
    up.embed_broken.store(true, Ordering::SeqCst);

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": MESSAGE}],
            "user_id": "u",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");

    let q = queue(&app.router).await;
    assert_eq!(q["total"], 1, "{q}");
    let row = &q["items"][0];
    assert_eq!(row["reason"], "provider_error");
    assert_eq!(row["messages"][0]["content"], MESSAGE);
    assert_eq!(
        last_operation(&app.router).await["failed_add_id"],
        row["id"]
    );

    up.embed_broken.store(false, Ordering::SeqCst);
    let out = retry(&app.router, json!({ "ids": [row["id"]] })).await;
    assert_eq!(out["stored"], 1, "{out}");
    assert_eq!(queue(&app.router).await["total"], 0);
}

#[tokio::test]
async fn an_entry_without_a_vector_is_kept_while_the_rest_is_stored() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    // Two entries, one of which the embedder refuses. The batch call fails as a whole, the
    // pipeline retries entry by entry, and the refused one is dropped without a trace.
    *up.answer.lock().unwrap() =
        json!({"memory": [{"id": "0", "text": "I write Rust"}, {"id": "1", "text": "poison pill"}]})
            .to_string();
    up.poison.lock().unwrap().replace("poison".to_string());

    let body = add(&app.router, MESSAGE).await;
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        1,
        "the entry that embedded is stored: {body}"
    );

    let q = queue(&app.router).await;
    assert_eq!(q["total"], 1, "{q}");
    let row = &q["items"][0];
    assert_eq!(row["reason"], "embed_failed");
    assert_eq!(last_operation(&app.router).await["reason"], "embed_failed");
    // Exactly the entry that was lost is kept — not both, and not neither.
    assert_eq!(row["items"].as_array().unwrap().len(), 1);
    assert_eq!(row["items"][0], "poison pill");

    // Still refused: the row stays, with the attempt counted.
    let out = retry(&app.router, json!({ "ids": [row["id"]] })).await;
    assert_eq!(out["stored"], 0, "{out}");
    assert_eq!(queue(&app.router).await["items"][0]["attempts"], 2);

    // Embedder repaired: the entry is stored from the text that was kept, without asking the
    // model for it again.
    up.poison.lock().unwrap().take();
    let out = retry(&app.router, json!({ "ids": [row["id"]] })).await;
    assert_eq!(out["stored"], 1, "{out}");

    let (_, memories) = call(&app.router, "GET", "/memories?user_id=u", None).await;
    let stored = memories["results"].as_array().unwrap();
    assert_eq!(stored.len(), 2, "{memories}");
    let stored = stored
        .iter()
        .find(|m| m["memory"] == "poison pill")
        .cloned()
        .expect("the retried entry is a memory now");
    // The retry replayed the text, not the conversation, but the memory still says where it
    // came from.
    let id = stored["id"].as_str().unwrap();
    let (_, inputs) = call(&app.router, "GET", &format!("/memories/{id}/inputs"), None).await;
    assert_eq!(inputs[0]["content"], MESSAGE, "{inputs}");
    assert_eq!(queue(&app.router).await["total"], 0);
}

#[tokio::test]
async fn an_entry_that_is_already_stored_counts_as_a_resolved_retry() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;

    // "poison pill" is a memory already — stored while the embedder worked.
    *up.answer.lock().unwrap() =
        json!({"memory": [{"id": "0", "text": "poison pill"}]}).to_string();
    add(&app.router, "I take a strange medicine").await;

    // Now a two-entry answer, one of which the embedder refuses.
    *up.answer.lock().unwrap() =
        json!({"memory": [{"id": "0", "text": "I write Rust"}, {"id": "1", "text": "poison pill"}]})
            .to_string();
    up.poison.lock().unwrap().replace("poison".to_string());
    add(&app.router, MESSAGE).await;

    let q = queue(&app.router).await;
    assert_eq!(q["total"], 1, "{q}");
    let row = q["items"][0].clone();
    assert_eq!(row["items"][0], "poison pill");

    // The embedder is fine again, but the text is already a memory: the fork's hash dedup
    // skips it, `add` answers with an empty result, and that is a finished retry — not a
    // failure that leaves the row stuck forever.
    up.poison.lock().unwrap().take();
    let out = retry(&app.router, json!({ "ids": [row["id"]] })).await;
    assert_eq!(out["stored"], 1, "{out}");
    assert_eq!(out["remaining"], 0, "{out}");

    let (_, memories) = call(&app.router, "GET", "/memories?user_id=u", None).await;
    let texts: Vec<&str> = memories["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["memory"].as_str())
        .collect();
    assert_eq!(
        texts.iter().filter(|t| **t == "poison pill").count(),
        1,
        "no duplicate was created: {memories}"
    );
}

#[tokio::test]
async fn an_add_whose_extraction_now_finds_nothing_also_resolves() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;

    // Store what extraction will later produce, then fail an add of the same message.
    *up.answer.lock().unwrap() =
        json!({"memory": [{"id": "0", "text": "I write Rust"}]}).to_string();
    add(&app.router, "first time").await;
    up.chat_broken.store(true, Ordering::SeqCst);
    add(&app.router, "first time").await;
    let row = queue(&app.router).await["items"][0].clone();

    // Repaired, but the entry now exists: the extraction path has nothing left to store, and
    // asking a third time would produce exactly the same nothing.
    up.chat_broken.store(false, Ordering::SeqCst);
    let out = retry(&app.router, json!({ "ids": [row["id"]] })).await;
    assert_eq!(out["remaining"], 0, "{out}");
    assert_eq!(queue(&app.router).await["total"], 0);
}

#[tokio::test]
async fn retrying_gives_up_after_three_consecutive_failures() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    up.chat_broken.store(true, Ordering::SeqCst);

    for content in ["first", "second", "third", "fourth"] {
        add(&app.router, content).await;
    }
    assert_eq!(queue(&app.router).await["total"], 4);

    // The upstream is still down: walking the whole queue would only collect identical errors.
    let out = retry(&app.router, json!({})).await;
    assert_eq!(out["retried"], 3, "{out}");
    assert_eq!(out["stored"], 0, "{out}");
    assert!(out["stopped_reason"].is_string(), "{out}");
    assert_eq!(
        out["remaining"], 4,
        "nothing was stored, so nothing was dropped"
    );

    // Attempts were counted, which is what tells the operator this has been tried. The queue
    // reads newest first and the batch takes the oldest three, so "first" is the last row.
    let q = queue(&app.router).await;
    let first = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["messages"][0]["content"] == "first")
        .cloned()
        .unwrap();
    assert_eq!(first["attempts"], 2, "{q}");
    let fourth = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["messages"][0]["content"] == "fourth")
        .cloned()
        .unwrap();
    assert_eq!(
        fourth["attempts"], 1,
        "the batch stopped before reaching it: {q}"
    );
}

#[tokio::test]
async fn deleting_the_queue_empties_it() {
    let _g = gate();
    let (up, base) = start().await;
    let app = app();
    configure(&app.router, &base).await;
    up.chat_broken.store(true, Ordering::SeqCst);
    add(&app.router, MESSAGE).await;
    add(&app.router, "another one").await;
    assert_eq!(queue(&app.router).await["total"], 2);

    let (status, body) = call(&app.router, "DELETE", "/memories/failed", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], 2, "{body}");
    assert_eq!(queue(&app.router).await["total"], 0);
}
