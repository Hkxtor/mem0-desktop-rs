//! Misc routes: `/api-keys*`, `/analytics/summary`, `/requests`, `/entities*`,
//! `/export`, `/import`, and the `/v1/entities` compat alias.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::Json;
use axum::Extension;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::AppState;

// ---------- /api-keys ----------

#[derive(Debug, Deserialize)]
pub struct CreateKeyBody {
    pub label: String,
}

fn generate_api_key() -> (String, String, String) {
    // (full_key, prefix, hash) — mirrors server/auth.py::generate_api_key
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let full = format!(
        "m0sk_{}",
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, raw)
    );
    let prefix = full.chars().take(10).collect::<String>();
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(full.as_bytes()));
    (full, prefix, hash)
}

pub async fn list_keys(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    let keys = store.list_api_keys()?;
    let out: Vec<Value> = keys
        .iter()
        .map(|k| {
            json!({
                "id": k.id,
                "key_prefix": k.key_prefix,
                "label": k.label,
                "created_at": k.created_at,
                "last_used_at": k.last_used_at,
                "is_active": k.is_active,
            })
        })
        .collect();
    let _ = auth;
    Ok(Json(json!(out)))
}

pub async fn create_key(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateKeyBody>,
) -> ApiResult<(axum::http::StatusCode, Json<Value>)> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let (full, prefix, hash) = generate_api_key();
    let owner = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.ensure_local_user()?;
        store
            .get_user_by_username("Local")?
            .map(|u| u.id)
            .or_else(|| auth.user_id.clone())
            .ok_or_else(|| ApiError::internal("no user to own the key"))?
    };
    let now = chrono::Utc::now().to_rfc3339();
    let row = crate::store::ApiKeyRow {
        id: uuid::Uuid::new_v4().to_string(),
        key_prefix: prefix.clone(),
        key_hash: hash,
        label: Some(body.label.clone()),
        created_by: owner,
        created_at: now.clone(),
        last_used_at: None,
        is_active: true,
    };
    {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.insert_api_key(&row)?;
    }
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "id": row.id,
            "key": full,
            "label": row.label,
            "key_prefix": prefix,
            "created_at": now,
        })),
    ))
}

pub async fn delete_key(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    if !store.delete_api_key(&id)? {
        return Err(ApiError::not_found("API key not found"));
    }
    Ok(Json(json!({ "message": "API key deleted" })))
}

// ---------- /analytics/summary, /requests ----------

#[derive(Debug, Deserialize)]
pub struct AnalyticsQuery {
    #[serde(default = "default_window")]
    pub window_days: i64,
}
fn default_window() -> i64 {
    12
}

pub async fn analytics_summary(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Query(q): Query<AnalyticsQuery>,
) -> ApiResult<Json<Value>> {
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    let summary = store.analytics_summary(q.window_days)?;
    Ok(Json(summary))
}

pub async fn list_requests(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    // Memory operations, not HTTP routes: the page shows what clients did to the
    // memories (ADD / SEARCH / UPDATE / DELETE), where an HTTP log would be mostly
    // /configure, /auth/refresh and dashboard polling.
    let operations = store.list_memory_operations(limit)?;
    Ok(Json(json!(operations)))
}

/// `GET /requests/stats`: the three totals above the list. Kept beside `/requests` rather
/// than folded into it, so the list keeps its plain-array shape.
pub async fn request_stats(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    Ok(Json(store.memory_operations_stats()?))
}

// ---------- /entities ----------

/// Entity buckets derived from memory payloads (user_id/agent_id/run_id), mirroring
/// `server/routers/entities.py::list_entities`.
pub async fn list_entities(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let mem = state
        .memory
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::unavailable("runtime_not_configured"))?;
    // Enumerate through the store: the vector table is a vec0 virtual table, so a plain
    // SQLite connection cannot read it (it reports `no such module: vec0`) and any such
    // query silently yields an empty page.
    let hits = mem
        .vector_store()
        .list(&Default::default(), Some(10_000))
        .await
        .map_err(ApiError::from)?;
    let entities = {
        let mut buckets: std::collections::BTreeMap<
            (String, String),
            (i64, Option<String>, Option<String>),
        > = Default::default();
        {
            for hit in &hits {
                let p = Value::Object(hit.payload.clone());
                for (etype, field) in [
                    ("user", "user_id"),
                    ("agent", "agent_id"),
                    ("run", "run_id"),
                ] {
                    if let Some(v) = p.get(field).and_then(|v| v.as_str()) {
                        if v.is_empty() {
                            continue;
                        }
                        let e = buckets
                            .entry((etype.to_string(), v.to_string()))
                            .or_default();
                        e.0 += 1;
                        if e.1.is_none() {
                            e.1 = p
                                .get("created_at")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                        }
                        if e.2.is_none() {
                            e.2 = p
                                .get("updated_at")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                        }
                    }
                }
            }
            buckets
                .into_iter()
                .map(|((t, id), (n, c, u))| json!({"id": id, "type": t, "total_memories": n, "created_at": c, "updated_at": u}))
                .collect::<Vec<Value>>()
        }
    };
    Ok(Json(json!(entities)))
}

pub async fn delete_entity(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path((etype, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let mem = state
        .memory
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::unavailable("runtime_not_configured"))?;
    let field = match etype.as_str() {
        "user" => "user_id",
        "agent" => "agent_id",
        "run" => "run_id",
        _ => {
            return Err(ApiError::bad_request(format!(
                "Unknown entity type '{etype}'"
            )))
        }
    };
    mem.delete_all(
        if field == "user_id" {
            Some(id.as_str())
        } else {
            None
        },
        if field == "agent_id" {
            Some(id.as_str())
        } else {
            None
        },
        if field == "run_id" {
            Some(id.as_str())
        } else {
            None
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(json!({ "message": "Entity deleted" })))
}

// ---------- /export, /import ----------

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    #[serde(default = "default_format")]
    pub format: String,
}
fn default_format() -> String {
    "json".into()
}

pub async fn export(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Query(q): Query<ExportQuery>,
) -> ApiResult<Json<Value>> {
    let mem = state
        .memory
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::unavailable("runtime_not_configured"))?;
    // Python exports `list_all_memories()["results"]` — the very rows the Memories page
    // shows. Reading the raw payload instead lost `id` and `memory`, and an export that
    // cannot be imported back is not an export.
    let memories: Vec<Value> = mem
        // include expired: Python's export reads the store directly and never filters them
        .list_all_include_expired(1000) // ALL_MEMORIES_LIMIT in server/memory_api.py
        .await
        .map_err(ApiError::from)?
        .get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    match q.format.as_str() {
        "json" | "pydantic" => Ok(Json(json!({
            "exported_at": chrono::Utc::now().to_rfc3339(),
            "total": memories.len(),
            "memories": memories,
        }))),
        "csv" => {
            // CSV is text/csv in Python; here we return the rows as JSON array (shape-only golden)
            Ok(Json(json!(memories)))
        }
        other => Err(ApiError::bad_request(format!(
            "Unsupported format '{other}'"
        ))),
    }
}

#[derive(Debug, Deserialize)]
pub struct ImportBody {
    pub memories: Vec<ImportItem>,
}

#[derive(Debug, Deserialize)]
pub struct ImportItem {
    pub memory: String,
    pub user_id: String,
    /// Preserved on import (official exports carry stable ids); re-importing the same id
    /// overwrites, keeping the import idempotent.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// YYYY-MM-DD; validated on add, stored in the payload, hidden from reads once expired.
    #[serde(default)]
    pub expiration_date: Option<String>,
    /// Official exports carry the platform's category labels; kept only if they are in
    /// the desktop preset (see crate::categories::PRESET_CATEGORIES).
    #[serde(default)]
    pub categories: Option<Vec<String>>,
}

pub async fn import(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<ImportBody>,
) -> ApiResult<Json<Value>> {
    let started = std::time::Instant::now();
    let detail = format!("{} memories", body.memories.len());
    let results_of = |v: &Json<Value>| v.0.get("imported").and_then(|n| n.as_i64());
    crate::http::ops::record(state.clone(), "IMPORT", detail, started, results_of, || {
        import_inner(state, auth, body)
    })
    .await
}

async fn import_inner(
    state: Arc<AppState>,
    auth: AuthContext,
    body: ImportBody,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let mem = state
        .memory
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::unavailable("runtime_not_configured"))?;
    let mut imported = 0usize;
    let mut failures: Vec<Value> = Vec::new();
    for item in &body.memories {
        if item.memory.trim().is_empty() || item.user_id.is_empty() {
            failures.push(json!({"memory": item.memory, "error": "empty memory or user_id"}));
            continue;
        }
        let mut metadata: mem0::JsonMap = item
            .metadata
            .clone()
            .map(|m| m.into_iter().collect())
            .unwrap_or_default();
        if let Some(cats) = &item.categories {
            let kept: Vec<Value> = cats
                .iter()
                .filter(|c| crate::categories::PRESET_CATEGORIES.contains(&c.as_str()))
                .map(|c| Value::String(c.clone()))
                .collect();
            metadata.insert("categories".to_string(), Value::Array(kept));
        }
        let opts = mem0::AddOptions {
            user_id: Some(item.user_id.clone()),
            infer: Some(false),
            metadata: Some(metadata),
            id: item.id.clone(),
            created_at: item.created_at.clone(),
            updated_at: item.updated_at.clone(),
            expiration_date: item.expiration_date.clone(),
            ..Default::default()
        };
        match mem.add(item.memory.clone(), opts).await {
            Ok(_) => imported += 1,
            Err(e) => failures.push(json!({"memory": item.memory, "error": e.to_string()})),
        }
    }
    Ok(Json(json!({
        "imported": imported,
        "skipped": failures.len(),
        "total_in_file": body.memories.len(),
        "failures": failures,
    })))
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route("/api-keys", get(list_keys).post(create_key))
        .route("/api-keys/{id}", delete(delete_key))
        .route("/analytics/summary", get(analytics_summary))
        .route("/requests", get(list_requests))
        .route("/requests/stats", get(request_stats))
        .route("/entities", get(list_entities))
        .route("/entities/{etype}/{id}", delete(delete_entity))
        .route("/v1/entities/", get(list_entities))
        .route("/v1/entities", get(list_entities))
        .route("/export", get(export))
        .route("/import", post(import))
}
