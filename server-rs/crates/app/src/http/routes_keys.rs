//! `GET /keys/status` — how the API key pools are doing.
//!
//! The Configuration page needs one line under each key field: how many keys are configured,
//! whether one is cooling down, whether one was rejected. It cannot read that from `/configure`,
//! which reports the *configuration* (and redacts credentials) — cooldowns are runtime state
//! that lives in the running clients.
//!
//! Admin-only, like `/diagnostics/storage` and `/settings/server`: it says nothing about a key's
//! contents, but it does describe the operator's credentials, and in a multi-user install that is
//! not the business of a non-admin account.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Json;
use axum::Extension;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::AppState;

pub async fn key_status(
    State(_state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }

    let mut out = serde_json::Map::new();
    for (section, status) in crate::pooled::snapshots() {
        out.insert(
            section.to_string(),
            json!({
                "keys": status.snapshot.keys,
                "cooling": status.snapshot.cooling,
                "revoked": status.snapshot.revoked,
                // Seconds until the earliest cooling key comes back; 0 when one is usable now.
                "next_available_in": status.snapshot.next_available_in,
                // True when this section spends the llm section's keys.
                "inherited": status.inherited,
                // The most recent refusal, already key-free ("429 rate limited").
                "last_error": status.snapshot.last_error,
            }),
        );
    }
    Ok(Json(Value::Object(out)))
}
