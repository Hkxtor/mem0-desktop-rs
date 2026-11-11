//! `GET /logs/app` and `GET /logs/requests` — the two records behind the Logs page.
//!
//! They answer different questions and are deliberately separate endpoints:
//!
//! - `/logs/app` is the application log, a JSONL file in the data directory. It explains
//!   *why* something happened (a provider error, a rejected config, a lost single-instance
//!   lock) and survives restarts, including a restart that never got as far as opening the
//!   database.
//! - `/logs/requests` is the HTTP traffic recorded in `request_logs`, which the Analytics
//!   figures are computed from.
//!
//! Both are admin-only. The application log can contain the model's own answer, truncated
//! but not paraphrased, which is memory content by another name; the request log carries the
//! paths every client asked for. Neither is something a non-admin user should read.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::Json;
use axum::Extension;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::AppState;

/// Default and maximum number of lines returned per call. The file is capped at 5 MB, so a
/// larger window is a bigger response rather than a deeper history.
const DEFAULT_APP_LINES: usize = 500;
const MAX_APP_LINES: usize = 5000;

/// The Requests page shows 100; the log tab is a diagnostic view and reads a wider window.
const DEFAULT_REQUEST_ROWS: i64 = 200;
const MAX_REQUEST_ROWS: i64 = 2000;

fn limit_param(q: &std::collections::HashMap<String, String>, default: usize, max: usize) -> usize {
    q.get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, max))
        .unwrap_or(default)
}

pub async fn app_log(
    State(_state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let limit = limit_param(&q, DEFAULT_APP_LINES, MAX_APP_LINES);
    let path = crate::logging::log_path();
    // An uninitialised writer is not an error: `--headless` in a test harness, or a launch
    // that failed before the data directory existed. Say so instead of returning an empty
    // list that reads like "nothing ever happened".
    let initialised = crate::logging::is_initialised();
    let lines = crate::logging::read_tail(limit);
    Ok(Json(json!({
        "lines": lines,
        "initialised": initialised,
        "path": path.map(|p| p.display().to_string()),
        "limit": limit,
    })))
}

pub async fn request_log(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let limit = limit_param(&q, DEFAULT_REQUEST_ROWS as usize, MAX_REQUEST_ROWS as usize) as i64;
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("state store lock poisoned"))?;
    let rows = store.list_request_logs(limit).map_err(ApiError::from)?;
    Ok(Json(json!({ "requests": rows, "limit": limit })))
}
