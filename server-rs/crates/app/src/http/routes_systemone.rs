//! `/systemone/*` routes: category classification backed by the `systemone` config
//! section (see `crate::categories`).
//!
//! - `GET  /systemone/categories` — the fixed preset + whether classification is on.
//! - `POST /systemone/classify`   — classify a text into a subset of the preset.
//!
//! These routes are Rust-desktop-only; they are not part of the Python server's route
//! enumeration, so they intentionally have no golden files.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Json;
use axum::Extension;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct ClassifyRequest {
    pub text: String,
}

/// Like [`classify_recorded`] but also returns the provider's raw answer map when the
/// backend has one (the SystemOne decision API does; chat models do not).
pub async fn classify_recorded_detailed(
    state: &AppState,
    classifier: &crate::categories::Classifier,
    text: &str,
) -> Result<(Vec<String>, Option<Value>), String> {
    let started = std::time::Instant::now();
    match classifier.classify_detailed(text).await {
        Ok((cats, raw)) => {
            note_classify_result(state, None);
            crate::logging::info(
                "systemone",
                "classified",
                serde_json::json!({
                    "categories": cats,
                    "latency_ms": started.elapsed().as_secs_f64() * 1000.0,
                }),
            );
            Ok((cats, raw))
        }
        Err(e) => {
            let msg = e.to_string();
            note_classify_result(state, Some(msg.clone()));
            crate::logging::warn(
                "systemone",
                "classification failed",
                serde_json::json!({
                    "error": msg,
                    "latency_ms": started.elapsed().as_secs_f64() * 1000.0,
                }),
            );
            Err(msg)
        }
    }
}

/// Record (or clear, with `None`) the reason the last classification failed.
pub fn note_classify_result(state: &AppState, error: Option<String>) {
    if let Ok(mut slot) = state.systemone_error.lock() {
        *slot = error;
    }
}

/// Classify `text`, recording the outcome so a failure is visible later. Returns the
/// categories on success, or the error message on failure — callers that treat
/// classification as best-effort (add) can ignore the error, but it is no longer lost.
pub async fn classify_recorded(
    state: &AppState,
    classifier: &crate::categories::Classifier,
    text: &str,
) -> Result<Vec<String>, String> {
    match classifier.classify(text).await {
        Ok(cats) => {
            note_classify_result(state, None);
            Ok(cats)
        }
        Err(e) => {
            let msg = e.to_string();
            note_classify_result(state, Some(msg.clone()));
            Err(msg)
        }
    }
}

/// The preset list plus whether a systemone model is configured.
pub async fn list_categories(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> Json<Value> {
    let configured = state.systemone.read().await.is_some()
        || crate::categories::is_configured(&super::routes_config::load_persisted_config(&state));
    Json(json!({
        "configured": configured,
        "categories": crate::categories::PRESET_CATEGORIES,
    }))
}

/// Classify a text. 400 on empty text, 503 when no systemone model is configured.
pub async fn classify(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    axum::Json(req): axum::Json<ClassifyRequest>,
) -> ApiResult<Json<Value>> {
    let text = req.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("text must not be empty"));
    }
    let classifier = super::classifier_or_restore(&state).await.ok_or_else(|| {
        ApiError::unavailable(
            "systemone_not_configured: no systemone model configured. \
             Add a \"systemone\" section to the configuration to enable categorization.",
        )
    })?;
    // A provider failure stays a 500 (it is not the caller's fault), but it is recorded
    // so the Configuration page can show it even when nobody calls this endpoint.
    match classifier.classify(text).await {
        Ok(categories) => {
            note_classify_result(&state, None);
            Ok(Json(json!({ "categories": categories })))
        }
        Err(e) => {
            note_classify_result(&state, Some(e.to_string()));
            Err(ApiError::from(e))
        }
    }
}

/// `POST /systemone/test` — a configuration smoke test the UI can show verbatim.
/// Uses a fixed probe sentence (no user data leaves the machine for this call) and
/// always answers 200, so the page can render the provider's own error text instead
/// of a generic failure.
/// The model actually in effect for the `systemone` section.
///
/// The config records what the operator typed, which is often nothing: a decision vendor
/// fills in its own default (`gemini-decision-1`), and a blank field is not a broken one.
/// Reporting the raw value as `null` made a working configuration look misconfigured in the
/// UI, which is exactly where someone goes to diagnose it.
fn effective_model(section: &Value) -> Value {
    if let Some(model) = section
        .get("config")
        .and_then(|c| c.get("model"))
        .and_then(|v| v.as_str())
        .filter(|m| !m.trim().is_empty())
    {
        return Value::String(model.to_string());
    }
    if let Some(cfg) = section.get("config") {
        if let Ok(client) = crate::decision::DecisionClient::from_config(cfg) {
            return Value::String(client.model().to_string());
        }
    }
    Value::Null
}

pub async fn test_classifier(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> Json<Value> {
    let cfg = super::routes_config::load_persisted_config(&state);
    let section = cfg.get("systemone").cloned().unwrap_or(Value::Null);
    let provider = section
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let model = effective_model(&section);
    if provider.is_empty() {
        return Json(json!({
            "ok": false,
            "configured": false,
            "error": "systemone_not_configured: choose a provider first",
        }));
    }
    let Some(classifier) = super::classifier_or_restore(&state).await else {
        return Json(json!({
            "ok": false,
            "configured": false,
            "provider": provider,
            "model": model,
            "error": "systemone_not_configured: the classifier could not be built",
        }));
    };
    match classify_recorded_detailed(&state, &classifier, PROBE_TEXT).await {
        Ok((categories, raw)) => Json(json!({
            "ok": true,
            "configured": true,
            "provider": provider,
            "model": model,
            "categories": categories,
            // The decision backend's per-question probabilities: this is what makes a
            // mis-tuned noul_threshold diagnosable instead of a mystery.
            "raw": raw,
        })),
        Err(error) => Json(json!({
            "ok": false,
            "configured": true,
            "provider": provider,
            "model": model,
            "error": error,
        })),
    }
}

/// Fixed sentence for `/systemone/test`: unambiguous enough that a working model
/// should tag it `technology`.
const PROBE_TEXT: &str = "I spent the weekend debugging a Rust program and then cooked ramen.";

/// Merge `categories` into a memory's payload without touching any other field
/// (the store's `update` replaces the whole payload, so read-merge-write), then
/// hand the merged payload back. Used by both add-time auto-classification and
/// `/systemone/classify-memory`.
pub async fn write_categories(
    mem: &Arc<mem0::Memory>,
    id: &str,
    categories: &[String],
) -> mem0::Result<()> {
    let store = mem.vector_store();
    let mut hit = store
        .get(id)
        .await?
        .ok_or_else(|| mem0::Mem0Error::not_found(format!("memory {id} not found")))?;
    hit.payload.insert(
        "categories".to_string(),
        serde_json::Value::Array(
            categories
                .iter()
                .map(|c| serde_json::Value::String(c.clone()))
                .collect(),
        ),
    );
    store.update(id, None, Some(hit.payload)).await
}

/// Re-classify an existing memory by id. 404 when the id does not exist.
pub async fn classify_memory(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    axum::Json(req): axum::Json<ClassifyMemoryRequest>,
) -> ApiResult<Json<Value>> {
    let mem = super::routes_config::memory_or_restore(&state)
        .await
        .ok_or_else(|| ApiError::unavailable("runtime_not_configured"))?;
    let store = mem.vector_store();
    let hit = store
        .get(&req.memory_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("memory {} not found", req.memory_id)))?;
    let text = hit
        .payload
        .get("data")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let classifier = super::classifier_or_restore(&state).await.ok_or_else(|| {
        ApiError::unavailable("systemone_not_configured: no systemone model configured")
    })?;
    let categories = classifier.classify(&text).await.map_err(ApiError::from)?;
    write_categories(&mem, &req.memory_id, &categories)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(
        json!({ "id": req.memory_id, "categories": categories }),
    ))
}

/// `POST /systemone/classify-batch` — classify up to `limit` memories that carry no
/// category yet, and report what is left so the caller can loop.
///
/// One provider call per memory (2-5s each), so the work is chunked: a whole store of
/// 800 unclassified memories is ~40 minutes and cannot live in a single request.
/// Stops after 3 consecutive provider failures — a dead quota must not be hammered
/// for the remaining hundreds.
pub async fn classify_batch(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    axum::Json(req): axum::Json<ClassifyBatchRequest>,
) -> ApiResult<Json<Value>> {
    let limit = req.limit.unwrap_or(20).clamp(1, 100) as usize;
    let mem = super::routes_config::memory_or_restore(&state)
        .await
        .ok_or_else(|| ApiError::unavailable("runtime_not_configured"))?;
    let classifier = super::classifier_or_restore(&state).await.ok_or_else(|| {
        ApiError::unavailable(
            "systemone_not_configured: no systemone model configured. \
             Add a \"systemone\" section to the configuration to enable categorization.",
        )
    })?;

    // `list_all` already hides expired memories — the same set the UI shows.
    let all = mem.list_all(10_000).await.map_err(ApiError::from)?;
    let skip: std::collections::HashSet<&str> = req.skip.iter().map(String::as_str).collect();
    let candidates: Vec<(String, String)> = all
        .get("results")
        .and_then(|r| r.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("categories")
                        .and_then(|c| c.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true)
                })
                .filter(|item| {
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| !skip.contains(id))
                        .unwrap_or(false)
                })
                .map(|item| {
                    (
                        item.get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        item.get("memory")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let total = candidates.len();
    let (mut processed, mut labeled, mut no_match, mut failed) = (0usize, 0usize, 0usize, 0usize);
    let mut consecutive_failures = 0usize;
    let mut no_match_ids: Vec<String> = Vec::new();
    let mut stopped_reason: Option<String> = None;

    for (id, text) in candidates.iter().take(limit) {
        processed += 1;
        match classify_recorded(&state, &classifier, text).await {
            Ok(categories) if !categories.is_empty() => {
                write_categories(&mem, id, &categories)
                    .await
                    .map_err(ApiError::from)?;
                labeled += 1;
                consecutive_failures = 0;
            }
            Ok(_) => {
                // The model answered and found nothing that fits. That is a real answer,
                // not a failure — reported separately so the UI can stop retrying it.
                no_match += 1;
                no_match_ids.push(id.clone());
                consecutive_failures = 0;
            }
            Err(e) => {
                failed += 1;
                consecutive_failures += 1;
                if consecutive_failures >= 3 {
                    stopped_reason = Some(e);
                    break;
                }
            }
        }
    }

    Ok(Json(json!({
        "processed": processed,
        "labeled": labeled,
        "no_match": no_match,
        "failed": failed,
        // Candidates left untouched: total minus what this call attempted.
        "remaining": total.saturating_sub(processed),
        "no_match_ids": no_match_ids,
        "stopped_reason": stopped_reason,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ClassifyBatchRequest {
    #[serde(default)]
    pub limit: Option<u32>,
    /// Ids the caller already tried this session and got no match for.
    #[serde(default)]
    pub skip: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClassifyMemoryRequest {
    pub memory_id: String,
}

/// Per-category memory counts across the whole store (preset keys only).
/// `{"configured": bool, "counts": {"technology": 60, ...}}` — counts only list
/// categories that actually occur; the page merges them over the 15-key preset.
pub async fn category_counts(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let configured = state.systemone.read().await.is_some()
        || crate::categories::is_configured(&super::routes_config::load_persisted_config(&state));
    let mut counts: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Some(mem) = super::routes_config::memory_or_restore(&state).await {
        // ALL_MEMORIES_LIMIT-style bound; desktop scale is <=10k rows.
        let all = mem
            .list_all_include_expired(10_000)
            .await
            .map_err(ApiError::from)?;
        if let Some(results) = all.get("results").and_then(|r| r.as_array()) {
            for item in results {
                if let Some(cats) = item.get("categories").and_then(|c| c.as_array()) {
                    for c in cats {
                        if let Some(name) = c.as_str() {
                            if crate::categories::PRESET_CATEGORIES.contains(&name) {
                                let entry =
                                    counts.entry(name.to_string()).or_insert_with(|| json!(0));
                                if let Some(n) = entry.as_u64() {
                                    counts.insert(name.to_string(), json!(n + 1));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // How many memories carry no category right now. Counted over the visible set only:
    // an expired memory is hidden from every list and search, so tagging it would spend
    // tokens on data nobody can see.
    let mut unclassified = 0u64;
    if let Some(mem) = super::routes_config::memory_or_restore(&state).await {
        if let Ok(all) = mem.list_all(10_000).await {
            if let Some(results) = all.get("results").and_then(|r| r.as_array()) {
                unclassified = results
                    .iter()
                    .filter(|item| {
                        item.get("categories")
                            .and_then(|c| c.as_array())
                            .map(|a| a.is_empty())
                            .unwrap_or(true)
                    })
                    .count() as u64;
            }
        }
    }
    let last_error = state
        .systemone_error
        .lock()
        .ok()
        .and_then(|slot| slot.clone());
    let cfg = super::routes_config::load_persisted_config(&state);
    let section = cfg.get("systemone").cloned().unwrap_or(Value::Null);
    Ok(Json(json!({
        "configured": configured,
        "counts": counts,
        "provider": section.get("provider").cloned().unwrap_or(Value::Null),
        "model": effective_model(&section),
        "last_error": last_error,
        "unclassified": unclassified,
    })))
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/systemone/categories", axum::routing::get(list_categories))
        .route("/systemone/classify", axum::routing::post(classify))
        .route(
            "/systemone/classify-memory",
            axum::routing::post(classify_memory),
        )
        .route(
            "/systemone/classify-batch",
            axum::routing::post(classify_batch),
        )
        .route("/systemone/counts", axum::routing::get(category_counts))
        .route("/systemone/test", axum::routing::post(test_classifier))
}
