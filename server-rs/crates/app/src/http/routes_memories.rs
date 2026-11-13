//! Memory routes: native `/memories*`, `/search`, `/reset` plus the `/v1` + `/v3`
//! SDK-compat aliases (each registered with and without trailing slash, mirroring
//! `server/routers/compat.py::_dual`).
//!
//! All handlers go through the M2 `Memory` in `AppState.memory`; before `/configure`
//! has succeeded they answer 503 with a `/setup` hint (the fresh-install state).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Json;
use axum::Extension;
use mem0::{AddOptions, Memory, SearchOptions};
use mem0_core::types::JsonMap;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::ops;
use super::AppState;

// ---------- request shapes (mirror server/memory_api.py pydantic models) ----------

#[derive(Debug, Deserialize)]
pub struct MessageIn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct MemoryCreate {
    pub messages: Vec<MessageIn>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
    #[serde(default)]
    pub infer: Option<bool>,
    #[serde(default)]
    pub memory_type: Option<String>,
    /// YYYY-MM-DD; stored on the memory and hidden from reads once past (UTC).
    #[serde(default)]
    pub expiration_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub filters: Option<JsonMap>,
    // deprecated top-level entity fields, folded into filters like Python does
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub threshold: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct MemoryUpdate {
    pub data: String,
    #[serde(default)]
    pub metadata: Option<JsonMap>,
}

#[derive(Debug, Deserialize)]
pub struct GetAllQuery {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub show_expired: Option<bool>,
    /// Comma-separated category names; AND semantics (a memory must carry every
    /// listed category). Sent by the dashboard when a category card is clicked.
    #[serde(default)]
    pub categories: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteAllQuery {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

// ---------- helpers ----------

async fn memory(state: &AppState) -> ApiResult<Arc<Memory>> {
    crate::http::routes_config::memory_or_restore(state)
        .await
        .ok_or_else(|| {
            // A build that was attempted and failed knows why; saying "no provider
            // configured" then sends the operator looking for a setting that is set.
            let detail = state
                .config_error
                .lock()
                .ok()
                .and_then(|e| e.clone())
                .map(|e| format!("Memory operations are not available yet: {e}"))
                .unwrap_or_else(|| {
                    "runtime_not_configured: no provider configured yet. Open /setup to configure one."
                        .to_string()
                });
            ApiError::unavailable(detail)
        })
}

fn build_filters(search: &SearchRequest) -> JsonMap {
    let mut filters = search.filters.clone().unwrap_or_default();
    for (k, v) in [
        ("user_id", &search.user_id),
        ("agent_id", &search.agent_id),
        ("run_id", &search.run_id),
    ] {
        if let Some(val) = v {
            filters.insert(k.to_string(), Value::String(val.clone()));
        }
    }
    filters
}

// ---------- native routes ----------

pub async fn add_memory(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Json(body): Json<MemoryCreate>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    let detail = ops::scope_detail(
        body.user_id.as_deref(),
        body.agent_id.as_deref(),
        body.run_id.as_deref(),
    );
    // The count depends on how many memories the extraction produced, so it comes from
    // the payload rather than from the request.
    let results_of = |v: &Json<Value>| ops::count_results(&v.0);
    // `add` swallows two failures by design (a provider error and an unparseable answer
    // both become `results: []`). The body writes the reason here so the Requests page can
    // explain a zero instead of leaving the operator to guess.
    let reason: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let reason_for_body = reason.clone();
    let reason_of = move |_: &Json<Value>| reason.lock().ok().and_then(|r| r.clone());
    ops::record_with_reason(
        state.clone(),
        "ADD",
        detail,
        started,
        results_of,
        reason_of,
        || add_memory_inner(state, body, reason_for_body),
    )
    .await
}

/// Stable codes stored in `memory_operations.reason`. The dashboard maps them to labels;
/// the provider's own message lives in the application log, not in a database column that
/// the UI would have to translate.
const REASON_NO_MEMORIES: &str = "no_memories";
const REASON_PROVIDER_ERROR: &str = "provider_error";
const REASON_UNPARSED: &str = "unparsed_answer";

async fn add_memory_inner(
    state: Arc<AppState>,
    body: MemoryCreate,
    reason: Arc<std::sync::Mutex<Option<String>>>,
) -> ApiResult<Json<Value>> {
    if body.user_id.is_none() && body.agent_id.is_none() && body.run_id.is_none() {
        return Err(ApiError::bad_request(
            "At least one identifier (user_id, agent_id, run_id) is required.",
        ));
    }
    let mem = memory(&state).await?;
    let messages: Vec<mem0::Message> = body
        .messages
        .iter()
        .map(|m| mem0::Message {
            role: m.role.clone(),
            content: m.content.clone(),
            name: None,
        })
        .collect();
    let opts = AddOptions {
        user_id: body.user_id.clone(),
        agent_id: body.agent_id.clone(),
        run_id: body.run_id.clone(),
        metadata: body.metadata.clone(),
        infer: body.infer,
        memory_type: body.memory_type.clone(),
        prompt: None,
        expiration_date: body.expiration_date.clone(),
        ..Default::default()
    };
    // Bracket the call so the extraction diagnostics belong to this add and not to a
    // concurrent one. See `mem0_core::diagnostics`.
    let diag_before = mem0_core::diagnostics::current_seq();
    let results = mem.add(messages, opts).await.map_err(ApiError::from)?;
    record_extraction_outcome(&results, diag_before, &reason);

    // Remember which messages produced each memory, so the detail panel can show the
    // source evidence. Best-effort: the add already happened and must not fail on this.
    let inputs: Vec<(Option<String>, String)> = body
        .messages
        .iter()
        .map(|m| (Some(m.role.clone()), m.content.clone()))
        .collect();
    if !inputs.is_empty() {
        if let Ok(store) = state.store.lock() {
            for r in &results {
                if let Err(e) = store.record_memory_inputs(&r.id, &inputs) {
                    eprintln!("recording inputs failed for {}: {e}", r.id);
                }
            }
        }
    }

    // Auto-categorize each new memory when a systemone model is configured. This is
    // best-effort: a classification failure yields categories=[] and never fails the
    // add itself (consensus rule 5).
    let classifier = super::classifier_or_restore(&state).await;
    let mut out: Vec<Value> = Vec::with_capacity(results.len());
    for r in &results {
        let categories = match &classifier {
            Some(clf) => {
                match super::routes_systemone::classify_recorded(&state, clf, &r.memory).await {
                    Ok(cats) => {
                        if let Err(e) =
                            super::routes_systemone::write_categories(&mem, &r.id, &cats).await
                        {
                            eprintln!("classify write-back failed for {}: {e}", r.id);
                        }
                        cats
                    }
                    Err(e) => {
                        // Best-effort by design: the memory is stored untagged. The error is
                        // recorded (not just logged) so the UI can show why categories are empty.
                        eprintln!("classify failed for {}: {e}", r.id);
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        // match Python's add() result shape: id/memory/event plus the actor fields
        out.push(json!({
            "id": r.id,
            "memory": r.memory,
            "event": r.event,
            "actor_id": body.agent_id.clone().or(body.run_id.clone()),
            "role": "user",
            "categories": categories,
        }));
    }
    Ok(Json(json!({ "results": out })))
}

/// Turn the extraction diagnostics into a log line and, when the add produced nothing, a
/// reason code for the Requests page.
fn record_extraction_outcome(
    results: &[mem0::AddResult],
    since: u64,
    reason: &Arc<std::sync::Mutex<Option<String>>>,
) {
    let diagnostics = mem0_core::diagnostics::since(since);
    if diagnostics.is_empty() {
        return;
    }
    // One add produces at most one diagnostic. Anything else means a concurrent add was
    // recorded in the same window: log them all, but let the first name this reason.
    for d in &diagnostics {
        let mut fields = serde_json::json!({
            "stage": d.stage.as_str(),
            "parsed": d.parsed_count,
            "error": d.error,
            "answer": d.answer,
            "truncated": d.answer.chars().count() > crate::logging::MAX_FIELD_CHARS,
        });
        // "unparsed" on its own is a dead end: the answer is cut at 2000 chars in the log,
        // so the part that failed to parse is exactly the part that never gets written down.
        // `json_error` names the failure ("EOF while parsing a string" is a truncated
        // answer, "expected `,`" is a malformed one) and the tail shows where it stopped.
        if d.stage == mem0_core::diagnostics::Stage::Unparsed {
            fields["answer_chars"] = serde_json::json!(d.answer.chars().count());
            fields["answer_tail"] = serde_json::json!(tail_chars(&d.answer, 300));
            fields["json_error"] = serde_json::json!(json_error(&d.answer));
        }
        crate::logging::warn("llm", "extraction produced no memories", fields);
    }
    if !results.is_empty() {
        return;
    }
    let Some(first) = diagnostics.first() else {
        return;
    };
    let code = match first.stage {
        mem0_core::diagnostics::Stage::LlmError => REASON_PROVIDER_ERROR,
        mem0_core::diagnostics::Stage::EmptyAnswer => REASON_NO_MEMORIES,
        mem0_core::diagnostics::Stage::Unparsed => REASON_UNPARSED,
        mem0_core::diagnostics::Stage::NoUsableEntries => REASON_UNPARSED,
    };
    if let Ok(mut slot) = reason.lock() {
        *slot = Some(code.to_string());
    }
}

/// Why an answer that looks like JSON did not parse. Cheap to compute and the only
/// information that distinguishes a truncated answer from a malformed one.
fn json_error(answer: &str) -> String {
    match serde_json::from_str::<Value>(answer) {
        Ok(_) => "parsed as JSON but carried no \"memory\" array".to_string(),
        Err(e) => e.to_string(),
    }
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

pub async fn search_memories(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Json(body): Json<SearchRequest>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    // The scope may arrive either as top-level fields or inside `filters`; report what
    // the search actually filtered on.
    let scoped = build_filters(&body);
    let field = |k: &str| {
        scoped
            .get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let detail = format!(
        "\"{}\" · {}",
        body.query,
        ops::scope_detail(
            field("user_id").as_deref(),
            field("agent_id").as_deref(),
            field("run_id").as_deref(),
        )
    );
    let results_of = |v: &Json<Value>| ops::count_results(&v.0);
    ops::record(state.clone(), "SEARCH", detail, started, results_of, || {
        search_memories_inner(state, body)
    })
    .await
}

async fn search_memories_inner(
    state: Arc<AppState>,
    body: SearchRequest,
) -> ApiResult<Json<Value>> {
    let mem = memory(&state).await?;
    let filters = build_filters(&body);
    let opts = SearchOptions {
        top_k: body.top_k.unwrap_or(20),
        threshold: body.threshold.unwrap_or(0.0),
        rerank: false,
    };
    let results = mem
        .search(&body.query, &filters, opts)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(results))
}

pub async fn get_memory(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    let ops_id = id.clone();
    ops::record(
        state.clone(),
        "GET",
        ops_id,
        started,
        |_: &Json<Value>| Some(1),
        || get_memory_inner(state, id),
    )
    .await
}

async fn get_memory_inner(state: Arc<AppState>, id: String) -> ApiResult<Json<Value>> {
    let mem = memory(&state).await?;
    let got = mem.get(&id).await.map_err(ApiError::from)?;
    match got {
        Some(v) => Ok(Json(v)),
        None => Err(ApiError::not_found(format!(
            "Memory with id {id} not found"
        ))),
    }
}

/// `GET /memories` — the dashboard's Memories page reads this, so it is deliberately not
/// recorded as a memory operation: opening a page is not something done to a memory, and
/// one row per visit would bury the adds and searches the Requests page exists for.
/// Parse the `categories` query param ("food,travel" -> ["food","travel"]); empty
/// segments are dropped.
fn parse_categories_param(raw: &Option<String>) -> Vec<String> {
    raw.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

pub async fn get_all_memories(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<GetAllQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    get_all_memories_inner(state, auth, q, headers).await
}

async fn get_all_memories_inner(
    state: Arc<AppState>,
    auth: AuthContext,
    q: GetAllQuery,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _ = headers;
    if q.user_id.is_none() && q.agent_id.is_none() && q.run_id.is_none() {
        // admin-only "list everything" path
        if !auth.is_admin() {
            return Err(ApiError::forbidden(
                "Admin role required to list all memories.",
            ));
        }
        // Admin "list everything" is a raw listing, not a filtered get_all (which
        // refuses to run without a scope). This is what the dashboard's Memories page
        // sends when its user filter is blank, so an empty answer here means an empty
        // page even though the memories exist.
        let mem = memory(&state).await?;
        // Python's unfiltered listing defaults to ALL_MEMORIES_LIMIT (1000).
        let top_k = q.top_k.unwrap_or(1000) as usize;
        let cats = parse_categories_param(&q.categories);
        let all = if cats.is_empty() {
            mem.list_all(top_k).await.map_err(ApiError::from)?
        } else {
            // category-only filter has no scope key, so get_all's scope requirement
            // does not apply — use the filtered unscoped listing instead.
            let mut filters = JsonMap::new();
            filters.insert("categories".into(), json!(cats));
            mem.list_all_filtered(&filters, top_k)
                .await
                .map_err(ApiError::from)?
        };
        return Ok(Json(all));
    }
    let mem = memory(&state).await?;
    let mut filters = JsonMap::new();
    if let Some(v) = &q.user_id {
        filters.insert("user_id".into(), json!(v));
    }
    if let Some(v) = &q.agent_id {
        filters.insert("agent_id".into(), json!(v));
    }
    if let Some(v) = &q.run_id {
        filters.insert("run_id".into(), json!(v));
    }
    let cats = parse_categories_param(&q.categories);
    if !cats.is_empty() {
        filters.insert("categories".into(), json!(cats));
    }
    let top_k = q.top_k.unwrap_or(100);
    let all = mem.get_all(&filters, top_k).await.map_err(ApiError::from)?;
    Ok(Json(all))
}

pub async fn update_memory(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<MemoryUpdate>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    let ops_id = id.clone();
    ops::record(
        state.clone(),
        "UPDATE",
        ops_id,
        started,
        |_: &Json<Value>| Some(1),
        || update_memory_inner(state, id, body),
    )
    .await
}

async fn update_memory_inner(
    state: Arc<AppState>,
    id: String,
    body: MemoryUpdate,
) -> ApiResult<Json<Value>> {
    let mem = memory(&state).await?;
    mem.update(&id, &body.data, body.metadata.clone())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "message": "Memory updated successfully!" })))
}

pub async fn delete_memory(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    let ops_id = id.clone();
    ops::record(
        state.clone(),
        "DELETE",
        ops_id,
        started,
        |_: &Json<Value>| Some(1),
        || delete_memory_inner(state, id),
    )
    .await
}

async fn delete_memory_inner(state: Arc<AppState>, id: String) -> ApiResult<Json<Value>> {
    let mem = memory(&state).await?;
    mem.delete(&id).await.map_err(ApiError::from)?;
    if let Ok(store) = state.store.lock() {
        let _ = store.delete_memory_inputs(&id);
    }
    Ok(Json(json!({ "message": "Memory deleted successfully" })))
}

/// `GET /memories/{id}/inputs` — the messages this memory was extracted from.
///
/// Empty for memories written before inputs were recorded, and for imported ones: the
/// official export carries no source evidence.
pub async fn memory_inputs(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let rows = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.list_memory_inputs(&id).map_err(ApiError::from)?
    };
    Ok(Json(json!(rows)))
}

pub async fn delete_all_memories(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<DeleteAllQuery>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    let detail = ops::scope_detail(
        q.user_id.as_deref(),
        q.agent_id.as_deref(),
        q.run_id.as_deref(),
    );
    ops::record(
        state.clone(),
        "DELETE_ALL",
        detail,
        started,
        |_: &Json<Value>| None,
        || delete_all_memories_inner(state, auth, q),
    )
    .await
}

async fn delete_all_memories_inner(
    state: Arc<AppState>,
    auth: AuthContext,
    q: DeleteAllQuery,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let mem = memory(&state).await?;
    mem.delete_all(
        q.user_id.as_deref(),
        q.agent_id.as_deref(),
        q.run_id.as_deref(),
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(
        json!({ "message": "All memories deleted successfully" }),
    ))
}

pub async fn memory_history(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mem = memory(&state).await?;
    let hist = mem.history(&id).await.map_err(ApiError::from)?;
    let v: Vec<Value> = hist
        .iter()
        .map(|h| serde_json::to_value(h).unwrap_or(Value::Null))
        .collect();
    Ok(Json(json!(v)))
}

pub async fn reset_memory(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    ops::record(
        state.clone(),
        "RESET",
        "-".to_string(),
        started,
        |_: &Json<Value>| None,
        || reset_memory_inner(state, auth),
    )
    .await
}

async fn reset_memory_inner(state: Arc<AppState>, auth: AuthContext) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let mem = memory(&state).await?;
    mem.reset().await.map_err(ApiError::from)?;
    if let Ok(store) = state.store.lock() {
        let _ = store.clear_memory_inputs();
    }
    Ok(Json(json!({ "message": "All memories reset" })))
}

// ---------- v1/v3 compat (dual trailing-slash registration) ----------

/// `GET /v1/ping` — MemoryClient's reachability probe; identifiers are constant for local.
pub async fn ping() -> Json<Value> {
    Json(json!({"org_id": "local", "project_id": "local", "user_email": null, "status": "ok"}))
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    let native = axum::Router::new()
        .route(
            "/memories",
            post(add_memory)
                .get(get_all_memories)
                .delete(delete_all_memories),
        )
        .route(
            "/memories/{id}",
            get(get_memory).put(update_memory).delete(delete_memory),
        )
        .route("/memories/{id}/history", get(memory_history))
        .route("/memories/{id}/inputs", get(memory_inputs))
        .route("/search", post(search_memories))
        .route("/reset", post(reset_memory));

    // /v1 and /v3 compat: same handlers, both slash spellings.
    let mut compat = axum::Router::new();
    compat = compat
        .route("/v1/ping/", get(ping))
        .route("/v1/ping", get(ping))
        .route("/v1/memories/search/", post(search_memories))
        .route("/v1/memories/search", post(search_memories))
        .route(
            "/v1/memories/",
            post(add_memory).delete(delete_all_memories),
        )
        .route("/v1/memories", post(add_memory).delete(delete_all_memories))
        .route(
            "/v1/memories/{id}/",
            get(get_memory).put(update_memory).delete(delete_memory),
        )
        .route(
            "/v1/memories/{id}",
            get(get_memory).put(update_memory).delete(delete_memory),
        )
        .route("/v1/memories/{id}/history/", get(memory_history))
        .route("/v1/memories/{id}/history", get(memory_history))
        .route("/v1/memories/{id}/inputs", get(memory_inputs))
        .route("/v3/memories/add/", post(add_memory))
        .route("/v3/memories/add", post(add_memory))
        .route("/v3/memories/search/", post(search_memories))
        .route("/v3/memories/search", post(search_memories))
        .route("/v3/memories/", post(add_memory))
        .route("/v3/memories", post(add_memory))
        .route("/v1/memories/", get(get_all_memories))
        .route("/v1/memories", get(get_all_memories));
    native.merge(compat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deprecated_top_level_fields_fold_into_filters() {
        let req = SearchRequest {
            query: "q".into(),
            filters: None,
            user_id: Some("alice".into()),
            agent_id: None,
            run_id: None,
            top_k: None,
            threshold: None,
        };
        let f = build_filters(&req);
        assert_eq!(f.get("user_id").and_then(|v| v.as_str()), Some("alice"));
    }

    #[test]
    fn add_requires_an_identifier() {
        // validated at handler level; here just confirm the struct parses a minimal body
        let body: MemoryCreate =
            serde_json::from_str(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
        assert!(body.user_id.is_none() && body.agent_id.is_none() && body.run_id.is_none());
    }

    /// A truncated answer and a malformed one both reach the Requests page as
    /// "回答无法解析", but they need different fixes. The log has to say which.
    #[test]
    fn a_truncated_answer_names_truncation() {
        let cut = r#"{"memory": [{"id": "0", "text": "User moved to Hangzhou"#;
        let err = json_error(cut);
        assert!(err.contains("EOF while parsing"), "{err}");
        assert_eq!(
            tail_chars(cut, 300),
            cut,
            "a short answer keeps its whole tail"
        );
        assert_eq!(tail_chars("abcdef", 3), "def");
    }

    #[test]
    fn a_malformed_answer_names_its_own_failure() {
        let err = json_error(r#"{"memory": [{"id": "0" "text": "x"}]}"#);
        assert!(err.contains("expected"), "{err}");
        assert!(!err.contains("EOF"), "not a truncation: {err}");
    }

    #[test]
    fn valid_json_without_a_memory_array_is_not_reported_as_a_parse_error() {
        let err = json_error(r#"{"memories": []}"#);
        assert!(err.contains("no \"memory\" array"), "{err}");
    }
}
