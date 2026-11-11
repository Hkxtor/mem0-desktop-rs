//! `GET /health` — liveness probe, excluded from the OpenAPI schema in Python too.

use axum::Json;
use serde_json::json;

pub async fn health() -> Json<serde_json::Value> {
    // Body is part of the golden API contract (Python answers exactly this); whether the
    // window is up is reported by /settings/desktop instead.
    Json(json!({ "status": "ok" }))
}

/// `POST /internal/close-window` — loopback-only. Asks the GUI shell to close its
/// window, which is the same path the window's close button takes (and therefore the
/// same close-to-tray decision). Headless has no window, so it is a 200 no-op.
pub async fn close_window() -> axum::Json<serde_json::Value> {
    // No receiver means the GUI has not finished starting; nothing to close.
    if super::close_bus().send(()).is_err() {
        eprintln!("close requested before the window was ready; ignored");
    }
    axum::Json(json!({ "message": "close requested" }))
}

/// `POST /internal/focus` — loopback-only. A second instance calls this on the
/// running owner to bring its window to the front. In headless mode there is no
/// window, so this is a 200 no-op (the receiver is simply never drained).
pub async fn focus(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<super::AppState>>,
) -> axum::Json<serde_json::Value> {
    let _ = state.focus_tx.send(());
    axum::Json(json!({ "message": "focus requested" }))
}
