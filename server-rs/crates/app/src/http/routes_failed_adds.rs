//! `GET /memories/failed`, `POST /memories/failed/retry`, `DELETE /memories/failed` — the
//! retry queue behind the Memories page.
//!
//! An `add` that stored nothing for a reason that may pass keeps its input (see
//! `docs/adr/0002-failed-add-retry.md`). These endpoints let an operator see that queue and
//! redo the work after fixing whatever was broken — a rate limit, a wrong key, a provider
//! outage — without the original client resubmitting anything, which it cannot: its request
//! is long gone.
//!
//! Admin-only, like `/logs/*` and `/diagnostics/storage`: the rows hold memory text verbatim,
//! which is the same material the logs are gated for.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::Json;
use axum::Extension;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::routes_memories::{add_memory_inner, MemoryCreate, MessageIn};
use super::AppState;

/// The drawer shows 50; a bigger window is a longer response, not a deeper queue, because the
/// table itself is capped at 500 rows.
const DEFAULT_ROWS: i64 = 50;
const MAX_ROWS: i64 = 500;

/// How many rows one `retry` call will attempt. The dashboard loops until the queue is empty,
/// so this is the batch size, not the ceiling.
const DEFAULT_BATCH: i64 = 20;

/// Stop after this many consecutive failures. A dead upstream fails every row the same way,
/// and walking a 500-row queue to collect 500 identical errors helps nobody — the same rule
/// batch classification uses.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

#[derive(Debug, Deserialize, Default)]
pub struct RetryRequest {
    /// Retry these rows specifically (the drawer's per-row button). Omitted means "the oldest
    /// `limit` pending rows".
    #[serde(default)]
    pub ids: Option<Vec<i64>>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn list_failed(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .map(|v| v.clamp(1, MAX_ROWS))
        .unwrap_or(DEFAULT_ROWS);
    let (items, total) = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        let items = store.list_failed_adds(limit).map_err(ApiError::from)?;
        let total = store.count_failed_adds().map_err(ApiError::from)?;
        (items, total)
    };
    Ok(Json(json!({ "items": items, "total": total })))
}

pub async fn delete_failed(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let removed = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.delete_all_failed_adds().map_err(ApiError::from)?
    };
    crate::logging::info(
        "store",
        "dropped the retry queue",
        json!({ "removed": removed }),
    );
    Ok(Json(json!({ "removed": removed })))
}

/// Re-run pending adds. Success deletes the row, because the messages then live on as that
/// memory's Input; a failure leaves it, with one more attempt recorded.
pub async fn retry_failed(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<RetryRequest>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let limit = body.limit.unwrap_or(DEFAULT_BATCH).clamp(1, MAX_ROWS);
    let ids = match body.ids {
        Some(ids) if !ids.is_empty() => ids,
        _ => {
            let store = state
                .store
                .lock()
                .map_err(|_| ApiError::internal("store lock poisoned"))?;
            store
                .pending_failed_add_ids(limit)
                .map_err(ApiError::from)?
        }
    };

    let mut retried = 0usize;
    let mut stored = 0usize;
    let mut failed = 0usize;
    let mut consecutive = 0usize;
    let mut stopped_reason: Option<String> = None;
    // One read of "what is already there" per scope, not per row: a batch of twenty retries used
    // to list the scope twenty times, and the entries a retry stored during the batch are added
    // to the cache as they land.
    let mut known: std::collections::HashMap<Scope, Vec<String>> = std::collections::HashMap::new();

    for id in ids {
        retried += 1;
        match retry_one(&state, id, &mut known).await {
            Ok(outcome) if outcome.resolved => {
                stored += outcome.stored;
                consecutive = 0;
            }
            Ok(_) => {
                // Still failing: the row kept its place, with the attempt counted by the
                // re-run itself.
                failed += 1;
                consecutive += 1;
            }
            Err(e) => {
                failed += 1;
                consecutive += 1;
                crate::logging::warn(
                    "store",
                    "retrying a failed add did not work",
                    json!({ "failed_add_id": id, "status": e.status.as_u16(), "error": e.detail }),
                );
            }
        }
        if consecutive >= MAX_CONSECUTIVE_FAILURES {
            stopped_reason = Some(format!(
                "{MAX_CONSECUTIVE_FAILURES} consecutive failures; the upstream is probably still down"
            ));
            break;
        }
    }

    let remaining = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.count_failed_adds().map_err(ApiError::from)?
    };
    Ok(Json(json!({
        "retried": retried,
        "stored": stored,
        "failed": failed,
        "remaining": remaining,
        "stopped_reason": stopped_reason,
    })))
}

struct Retried {
    stored: usize,
    /// The work is finished: the row is gone, whether it stored something, found nothing left
    /// to store, or found its text already present.
    resolved: bool,
}

/// Redo one pending add.
///
/// Two shapes, because the two failure families left different material behind:
///
/// - **Entries existed** (an embedding failure): embed and store them verbatim, then give each
///   new memory the original conversation as its Input. Re-extracting would spend a model call
///   to produce text the operator never saw and did not ask to retry.
/// - **No entries** (a provider error or an unparseable answer): replay the add, which runs
///   extraction again against whatever config is current now.
///
/// A row is deleted only when the retry actually stored something. If a partial success left
/// leftovers, the re-run has already rewritten the same row (same dedup key) with what is
/// still missing.
async fn retry_one(
    state: &Arc<AppState>,
    id: i64,
    known: &mut std::collections::HashMap<Scope, Vec<String>>,
) -> ApiResult<Retried> {
    let row = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.get_failed_add(id).map_err(ApiError::from)?
    };
    let Some(row) = row else {
        // Deleted between the list call and this one: nothing to do, and not an error.
        return Ok(Retried {
            stored: 0,
            resolved: true,
        });
    };

    let items: Vec<String> = row["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let original: Vec<MessageIn> = row["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| MessageIn {
                    role: m["role"].as_str().unwrap_or("user").to_string(),
                    content: m["content"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let scope = (
        row["user_id"].as_str().map(|s| s.to_string()),
        row["agent_id"].as_str().map(|s| s.to_string()),
        row["run_id"].as_str().map(|s| s.to_string()),
    );
    let metadata = row["metadata"].as_object().cloned();
    let memory_type = row["memory_type"].as_str().map(|s| s.to_string());
    let expiration_date = row["expiration_date"].as_str().map(|s| s.to_string());
    let infer = row["infer"].as_bool().unwrap_or(true);

    let build = |messages: Vec<MessageIn>, infer: bool| MemoryCreate {
        messages,
        user_id: scope.0.clone(),
        agent_id: scope.1.clone(),
        run_id: scope.2.clone(),
        metadata: metadata.clone(),
        infer: Some(infer),
        memory_type: memory_type.clone(),
        expiration_date: expiration_date.clone(),
    };

    let (stored, leftovers) = if items.is_empty() {
        // Nothing was extracted last time, so the add itself has to run again.
        let slot = Arc::new(std::sync::Mutex::new(super::ops::Outcome::default()));
        let payload = add_memory_inner(
            state.clone(),
            build(original.clone(), infer),
            slot.clone(),
            true,
        )
        .await?
        .0;
        let stored = payload["results"].as_array().map(|a| a.len()).unwrap_or(0);
        let outcome = slot.lock().ok().map(|o| o.clone()).unwrap_or_default();
        if outcome.failed_add_id == Some(id) {
            // The same failure happened again: the row kept itself, with the attempt counted.
            return Ok(Retried {
                stored,
                resolved: false,
            });
        }
        if stored == 0 {
            // The call completed but produced nothing to store. Either the model now finds
            // nothing memorable, or the entries it produced are already stored — asking again
            // cannot change either. Not a failure, but worth a line: the row's input is gone
            // after this, exactly as the operator asked.
            crate::logging::info(
                "store",
                "a retried add had nothing left to store",
                json!({ "failed_add_id": id, "reason": outcome.reason }),
            );
        }
        (stored, Vec::new())
    } else {
        let mut stored = 0usize;
        let mut leftovers: Vec<String> = Vec::new();
        // Read once per scope: an entry whose text is already a memory is a resolved retry, not a
        // failure, and `add` reports both cases as an empty result.
        if !known.contains_key(&scope) {
            let texts = stored_texts(state, &scope).await;
            known.insert(scope.clone(), texts);
        }
        // Filled on the line above, so this cannot be absent; no unwrap on a request path.
        let Some(present) = known.get_mut(&scope) else {
            return Err(ApiError::internal("retry scope cache was not filled"));
        };
        for text in &items {
            if present.iter().any(|t| t == text) {
                // The text became a memory between the failure and now (the caller resent it,
                // or an earlier retry stored it). `infer: false` inserts unconditionally, so
                // calling `add` here would create a duplicate rather than a no-op.
                stored += 1;
                continue;
            }
            let single = vec![MessageIn {
                role: "user".to_string(),
                content: text.clone(),
            }];
            // `infer: false`: the text is already the memory. Nothing to extract.
            let slot = Arc::new(std::sync::Mutex::new(super::ops::Outcome::default()));
            match add_memory_inner(state.clone(), build(single, false), slot, false).await {
                Ok(payload) => {
                    let ids: Vec<String> = payload.0["results"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|r| r["id"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if ids.is_empty() {
                        // Nothing stored and no error: the entry was dropped inside the
                        // pipeline. It is not in the store, so the row has to keep it.
                        leftovers.push(text.clone());
                        continue;
                    }
                    stored += ids.len();
                    present.push(text.clone());
                    // The Input panel should show the conversation this memory came from, not
                    // the text the retry replayed.
                    let inputs: Vec<(Option<String>, String)> = original
                        .iter()
                        .map(|m| (Some(m.role.clone()), m.content.clone()))
                        .collect();
                    if let Ok(store) = state.store.lock() {
                        for mid in &ids {
                            if let Err(e) = store.record_memory_inputs(mid, &inputs) {
                                crate::logging::warn(
                                    "store",
                                    "could not attach the original input to a retried memory",
                                    json!({ "memory_id": mid, "error": e.to_string() }),
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    crate::logging::warn(
                        "store",
                        "retrying an entry that had no vector did not work",
                        json!({ "failed_add_id": id, "status": e.status.as_u16(), "error": e.detail }),
                    );
                    leftovers.push(text.clone());
                }
            }
        }
        (stored, leftovers)
    };

    if !leftovers.is_empty() {
        // Put back only what is still missing, on the same row.
        if let Ok(store) = state.store.lock() {
            let messages: Vec<Value> = original
                .iter()
                .map(|m| json!({ "role": m.role, "content": m.content }))
                .collect();
            let key = super::routes_memories::failed_add_key_for(
                &messages,
                scope.0.as_deref(),
                scope.1.as_deref(),
                scope.2.as_deref(),
            );
            let _ = store.record_failed_add(
                &key,
                "embed_failed",
                (scope.0.as_deref(), scope.1.as_deref(), scope.2.as_deref()),
                metadata.as_ref().map(|m| Value::Object(m.clone())).as_ref(),
                &messages,
                &leftovers,
                None,
                (
                    Some(infer),
                    memory_type.as_deref(),
                    expiration_date.as_deref(),
                ),
            );
        }
        return Ok(Retried {
            stored,
            resolved: false,
        });
    }

    {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.delete_failed_add(id).map_err(ApiError::from)?;
    }
    crate::logging::info(
        "store",
        "a retried add was resolved",
        json!({ "failed_add_id": id, "stored": stored }),
    );
    Ok(Retried {
        stored,
        resolved: true,
    })
}

/// Which scope a retry belongs to. Hashable so a batch can cache one read per scope.
type Scope = (Option<String>, Option<String>, Option<String>);

/// The texts already stored for a scope, for telling "already there" from "lost".
async fn stored_texts(
    state: &Arc<AppState>,
    scope: &(Option<String>, Option<String>, Option<String>),
) -> Vec<String> {
    let Ok(mem) = super::routes_memories::memory(state).await else {
        return Vec::new();
    };
    let mut filters = mem0_core::types::JsonMap::new();
    for (key, value) in [
        ("user_id", &scope.0),
        ("agent_id", &scope.1),
        ("run_id", &scope.2),
    ] {
        if let Some(v) = value {
            filters.insert(key.to_string(), json!(v));
        }
    }
    let Ok(all) = mem.get_all(&filters, 1000).await else {
        return Vec::new();
    };
    all.get("results")
        .and_then(|r| r.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|m| m["memory"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{delete, post};
    axum::Router::new()
        .route("/memories/failed", axum::routing::get(list_failed))
        .route("/memories/failed", delete(delete_failed))
        .route("/memories/failed/retry", post(retry_failed))
}
