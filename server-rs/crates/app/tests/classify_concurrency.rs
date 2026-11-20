//! Classification happens after the add has been answered, and it happens one memory at a time.
//!
//! The invariant used to be the opposite: classification ran inside the add, four at a time. It
//! moved to a background worker (`crate::classify_queue`) because the add was paying 1383 ms of
//! a real 11017 ms for tags the response never carried. So what is asserted here changed — the
//! add must return without tags, and the worker must then tag every memory *serially*, because
//! it spends the same provider keys the extraction just used.
//!
//! The stall in the mock classifier is what makes both halves observable: an add that still
//! waited would sit there, and a concurrent worker would show a peak above one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

/// How many memories one add extracts.
const EXTRACTED: usize = 6;
/// Each classification takes this long. Serial, six of them cost 6×; concurrent, about 2×.
const CLASSIFY_STALL: Duration = Duration::from_millis(400);

async fn llm(Json(_body): Json<Value>) -> Json<Value> {
    let memories: Vec<Value> = (0..EXTRACTED)
        .map(|i| json!({"id": i.to_string(), "text": format!("fact number {i}")}))
        .collect();
    Json(json!({
        "choices": [{
            "message": {"role": "assistant", "content": json!({"memory": memories}).to_string()},
            "finish_reason": "stop",
            "index": 0,
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }))
}

async fn embed(Json(body): Json<Value>) -> Json<Value> {
    let n = body["input"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(1)
        .max(1);
    let data: Vec<Value> = (0..n)
        .map(|i| json!({"embedding": vec![0.1f32; 8], "index": i}))
        .collect();
    Json(json!({"data": data}))
}

/// How many classification requests are being answered right now, and the most that ever were.
///
/// The wall clock alone is a load-sensitive way to say "this is concurrent": on a busy machine
/// (or with cargo running several test binaries at once) a correct implementation can miss a
/// generous deadline, and a serial one can still pass when the box is idle and fast. Counting
/// in-flight requests says it directly and cannot flake.
#[derive(Default)]
struct InFlight {
    now: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
}

impl InFlight {
    fn enter(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.total.fetch_add(1, Ordering::SeqCst);
    }
    /// How many classification requests have been answered so far.
    fn total(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }
    fn leave(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

/// The Gemini decision protocol: one request per criterion, stalled on purpose.
async fn decisions(
    State(inflight): State<Arc<InFlight>>,
    _headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    inflight.enter();
    tokio::time::sleep(CLASSIFY_STALL).await;
    inflight.leave();
    // `answers` is a map keyed by category name, not an array — one entry per question asked.
    let mut answers = serde_json::Map::new();
    for name in ["technology", "food", "hobbies", "professional_details"] {
        answers.insert(
            name.to_string(),
            json!({"type": "noul", "noul": 0.9, "confidence": 0.9}),
        );
    }
    let _ = body;
    Json(json!({"answers": Value::Object(answers)}))
}

async fn start() -> (String, Arc<InFlight>) {
    let inflight = Arc::new(InFlight::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(llm))
        .route("/v1/embeddings", post(embed))
        .route("/v1beta/decisions", post(decisions))
        .with_state(inflight.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), inflight)
}

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
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

/// Poll until every memory in `ids` carries a category, or give up.
///
/// Background work means "not yet" is a normal state, so the assertion has to be about where it
/// ends up rather than about one immediate read. 30 s is far longer than six stalled
/// classifications need; the point is that a slow CI machine cannot make this flaky.
async fn wait_for_categories(app: &Router, ids: &[String]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let (_, listed) = call(app, "GET", "/memories?user_id=alice&top_k=50", None).await;
        let rows = listed["results"].as_array().cloned().unwrap_or_default();
        let tagged = ids.iter().all(|id| {
            rows.iter().any(|r| {
                r["id"] == json!(id)
                    && r["categories"]
                        .as_array()
                        .map(|a| !a.is_empty())
                        .unwrap_or(false)
            })
        });
        if tagged {
            return listed;
        }
        last = listed;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("memories never got their categories within 30s: {last}");
}

#[tokio::test]
async fn an_add_returns_before_classification_and_the_worker_is_serial() {
    let (base, inflight) = start().await;
    let app = app();
    let (status, body) = call(
        &app.router,
        "POST",
        "/configure",
        Some(json!({
            "llm": {"provider": "openai", "config": {
                "model": "gpt-4o", "api_key": "sk-a", "openai_base_url": format!("{base}/v1"),
            }},
            "embedder": {"provider": "openai", "config": {
                "model": "text-embedding-3-small", "api_key": "sk-a",
                "openai_base_url": format!("{base}/v1"), "embedding_dims": 8,
            }},
            "vector_store": {"provider": "sqlite-vec",
                             "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
            "systemone": {"provider": "systemone", "config": {
                "api_key": "sk-a",
                "base_url": format!("{base}/v1beta/decisions"),
                "model": "gemini-decision-1",
            }},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let started = Instant::now();
    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "six facts"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    let add_wall = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");

    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), EXTRACTED, "{body}");
    let ids: Vec<String> = results
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();

    // The point of the whole change: the response does not carry tags. Not "some are missing" —
    // every one is empty, because classification had not started when the response was built.
    for r in results {
        assert_eq!(
            r["categories"],
            json!([]),
            "the add must not wait for classification: {body}"
        );
    }
    // Six stalled classifications are 2.4 s of work; the add has to be a small fraction of it.
    assert!(
        add_wall < CLASSIFY_STALL * (EXTRACTED as u32) / 2,
        "the add took {add_wall:?}, which means it waited for classification"
    );

    // ...and the tags do arrive, on every row.
    let listed = wait_for_categories(&app.router, &ids).await;

    // Serial on purpose: one classifier call at a time, however many memories are queued.
    let peak = inflight.peak();
    assert_eq!(
        peak, 1,
        "the worker must classify one memory at a time, saw {peak} at once: {listed}"
    );
    assert_eq!(
        inflight.total(),
        EXTRACTED,
        "every memory should have been classified exactly once"
    );
    println!(
        "add={add_wall:?} for {EXTRACTED} memories, {} classifications done serially",
        inflight.total()
    );
}

#[tokio::test]
async fn categories_land_on_the_row_they_belong_to() {
    let (base, _inflight) = start().await;
    let app = app();
    let (status, _) = call(
        &app.router,
        "POST",
        "/configure",
        Some(json!({
            "llm": {"provider": "openai", "config": {
                "model": "gpt-4o", "api_key": "sk-a", "openai_base_url": format!("{base}/v1"),
            }},
            "embedder": {"provider": "openai", "config": {
                "model": "text-embedding-3-small", "api_key": "sk-a",
                "openai_base_url": format!("{base}/v1"), "embedding_dims": 8,
            }},
            "vector_store": {"provider": "sqlite-vec",
                             "config": {"collection_name": "mem0", "embedding_model_dims": 8}},
            "systemone": {"provider": "systemone", "config": {
                "api_key": "sk-a",
                "base_url": format!("{base}/v1beta/decisions"),
                "model": "gemini-decision-1",
            }},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app.router,
        "POST",
        "/memories",
        Some(json!({
            "messages": [{"role": "user", "content": "six facts"}],
            "user_id": "alice",
            "infer": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results = body["results"].as_array().unwrap();
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["memory"], json!(format!("fact number {i}")), "{body}");
    }
}
