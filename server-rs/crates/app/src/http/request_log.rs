//! Request logging (`log_requests` in server/main.py).
//!
//! Every served request lands in `request_logs`, which is what the dashboard's Requests
//! page and its analytics read. Without this middleware both pages are empty forever:
//! nothing else writes those rows.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::http::{auth::AuthTypeSlot, AppState};

/// Paths that are not API traffic, or whose logging would change the numbers they report.
const SKIPPED_PATHS: &[&str] = &["/api/health", "/health", "/docs", "/redoc", "/openapi.json"];

/// Dashboard assets are not API traffic; reading the statistics must not add to them,
/// so `/requests` and `/analytics` are excluded too (Python's SKIPPED_REQUEST_LOG_PREFIXES).
const SKIPPED_PREFIXES: &[&str] = &[
    "/requests",
    "/analytics",
    "/_next",
    "/fonts",
    "/images",
    "/icons",
    "/dashboard",
];

fn should_log(method: &str, path: &str) -> bool {
    if method == "OPTIONS" {
        return false;
    }
    if SKIPPED_PATHS.contains(&path) {
        return false;
    }
    !SKIPPED_PREFIXES.iter().any(|p| path.starts_with(p))
}

pub async fn log_requests(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    let slot = AuthTypeSlot::default();
    request.extensions_mut().insert(slot.clone());

    let started = Instant::now();
    let response = next.run(request).await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let status = response.status().as_u16() as i64;

    if should_log(&method, &path) {
        let auth_type = slot.get();
        // A locked store must not take down the response path: logging is diagnostics.
        if let Ok(store) = state.store.lock() {
            if let Err(e) = store.log_request(&method, &path, status, latency_ms, auth_type) {
                eprintln!("failed to persist request log: {e}");
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::should_log;

    #[test]
    fn skips_probes_options_and_the_pages_that_read_the_log() {
        assert!(!should_log("GET", "/health"));
        assert!(!should_log("OPTIONS", "/memories"));
        // reading the stats must not change them
        assert!(!should_log("GET", "/requests"));
        assert!(!should_log("GET", "/analytics/summary"));
        // dashboard assets
        assert!(!should_log("GET", "/_next/static/chunk.js"));
        assert!(!should_log("GET", "/dashboard/memories"));
        // real API traffic
        assert!(should_log("POST", "/memories"));
        assert!(should_log("GET", "/memories"));
        assert!(should_log("POST", "/configure"));
        assert!(should_log("GET", "/export"));
    }
}
