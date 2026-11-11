//! Recording what happened to the memories, for the dashboard's Requests page.
//!
//! That page answers "what did clients do with my memories" — added, searched, changed,
//! deleted — not "which HTTP routes were hit". The two are different questions: an HTTP
//! log is dominated by `/configure`, `/auth/refresh` and dashboard polling, while the
//! memory actions are what an operator opens the page for.

use std::sync::Arc;
use std::time::Instant;

use axum::response::Json;
use serde_json::Value;

use super::error::ApiError;
use super::AppState;

/// Render a scope (`user:alice · agent:support`) for the detail column.
pub fn scope_detail(user: Option<&str>, agent: Option<&str>, run: Option<&str>) -> String {
    let mut parts = Vec::new();
    for (label, value) in [("user", user), ("agent", agent), ("run", run)] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            parts.push(format!("{label}:{v}"));
        }
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Number of entries in a `{"results": [...]}` payload.
pub fn count_results(payload: &Value) -> Option<i64> {
    payload
        .get("results")
        .and_then(|r| r.as_array())
        .map(|a| a.len() as i64)
}

fn persist(
    state: &AppState,
    operation: &str,
    detail: &str,
    results: Option<i64>,
    status: i64,
    latency_ms: f64,
) {
    // Logging must never turn a successful memory call into an error: this is bookkeeping.
    if let Ok(store) = state.store.lock() {
        if let Err(e) = store.log_memory_operation(operation, detail, results, status, latency_ms) {
            eprintln!("failed to record memory operation: {e}");
        }
    }
}

/// Run a handler body and record the outcome either way.
///
/// `results_of` extracts the count from a successful payload, so an ADD can report how
/// many memories it produced and a SEARCH how many it returned.
pub async fn record<T, F, Fut, C>(
    state: Arc<AppState>,
    operation: &'static str,
    detail: String,
    started: Instant,
    results_of: C,
    body: F,
) -> Result<T, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
    C: Fn(&T) -> Option<i64>,
{
    let outcome = body().await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    match &outcome {
        Ok(payload) => persist(
            &state,
            operation,
            &detail,
            results_of(payload),
            200,
            latency_ms,
        ),
        Err(e) => persist(
            &state,
            operation,
            &detail,
            None,
            e.status.as_u16() as i64,
            latency_ms,
        ),
    }
    outcome
}

/// Record an operation that does not return a payload worth counting.
pub async fn record_unit<F, Fut>(
    state: Arc<AppState>,
    operation: &'static str,
    detail: String,
    started: Instant,
    results: Option<i64>,
    body: F,
) -> Result<Json<Value>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Json<Value>, ApiError>>,
{
    record(state, operation, detail, started, move |_| results, body).await
}
