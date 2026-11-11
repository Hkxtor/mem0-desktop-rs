//! `/configure*` routes: get (redacted), set (via M2 compute_updated_config +
//! build_memory, reject-on-failure), bundled provider list, and
//! `/generate-instructions`.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Json;
use axum::Extension;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::AppState;
use crate::desktop_settings::{apply_autostart, DesktopSettings, AUTOSTART_SUPPORTED};

// "mock" is bundled in the desktop build for tests/e2e (the fork ships it); the Python
// product rejects it because it is not a real provider.
pub const BUNDLED_LLM_PROVIDERS: &[&str] = &["openai", "anthropic", "gemini", "mock"];
pub const BUNDLED_EMBEDDER_PROVIDERS: &[&str] = &["openai", "gemini", "mock"];
/// Providers the `systemone` (category classifier) section may use. Same set as llm.
/// `systemone` is the SystemOne decision API (state + typed questions), not a chat model;
/// see `crate::decision`. The rest are OpenAI-compatible chat providers.
pub const BUNDLED_SYSTEMONE_PROVIDERS: &[&str] =
    &["openai", "anthropic", "gemini", "systemone", "mock"];

/// Keys whose values are credentials, whatever the spelling (`server/main.py::_is_sensitive_key`).
fn is_sensitive_key(key: &str) -> bool {
    // Insert '_' at camelCase boundaries (lower/digit -> Upper), then normalize, matching
    // Python's regex (?<=[a-z0-9])(?=[A-Z]) plus lowercase fold.
    let mut spaced = String::new();
    let chars: Vec<char> = key.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            if prev.is_ascii_lowercase() || prev.is_ascii_digit() {
                spaced.push('_');
            }
        }
        spaced.push(*c);
    }
    let normalized: String = spaced
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('_');
    // substrings that mark a credential: api_key, token, secret, password, credential
    [
        "api_key",
        "token",
        "secret",
        "password",
        "credential",
        "private_key",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

/// Recursively replace credential values with "[redacted]" (`_redact_config`).
pub fn redact_config(value: &Value, key: Option<&str>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_config(v, Some(k))))
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(|v| redact_config(v, key)).collect()),
        other => {
            if let Some(k) = key {
                if is_sensitive_key(k) {
                    if other.is_null() || other.as_str() == Some("") {
                        return other.clone();
                    }
                    return Value::String("[redacted]".into());
                }
            }
            other.clone()
        }
    }
}

/// The live config JSON, persisted in `{data_dir}/config.json` so a restart keeps it.
fn config_path(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join("config.json")
}

pub fn load_persisted_config(state: &AppState) -> Value {
    std::fs::read_to_string(config_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Object(Map::new()))
}

pub fn persist_config(state: &AppState, config: &Value) -> ApiResult<()> {
    std::fs::write(
        config_path(state),
        serde_json::to_string_pretty(config).map_err(|e| ApiError::internal(e.to_string()))?,
    )
    .map_err(|e| ApiError::internal(format!("write config: {e}")))?;
    Ok(())
}

/// Rebuild the runtime from the persisted config at startup.
///
/// Mirrors `server/server_state.py::initialize_state`: a launch with a stored, valid
/// configuration must come back up *serving memory requests*, not asking the operator to
/// configure the same providers again. Without this, every memory endpoint answers
/// 503 `runtime_not_configured` after a restart even though `config.json` is on disk.
///
/// An unusable configuration must not stop the process — a portable app has to keep
/// `/health` and `/setup` reachable so the credentials can be fixed (the same reason
/// Python records `_initialization_error` instead of raising).
pub async fn restore_persisted_runtime(state: &AppState) {
    let _ = memory_or_restore(state).await;
}

/// The configured runtime, rebuilding it from the persisted config when the slot is empty.
///
/// Two callers depend on this: startup (a restart must come back serving, not asking to be
/// configured again) and the memory endpoints (so a launch whose startup build failed — or a
/// wizard run where the prefilled form was not "dirty" and never re-posted `/configure` —
/// recovers on the next request instead of answering 503 forever). A fresh install has no
/// persisted config, so this stays `None` and the wizard still owns the first configuration.
pub async fn memory_or_restore(state: &AppState) -> Option<std::sync::Arc<mem0::Memory>> {
    if let Some(memory) = state.memory.read().await.clone() {
        return Some(memory);
    }
    let cfg = load_persisted_config(state);
    if cfg.as_object().map(|m| m.is_empty()).unwrap_or(true) {
        return None; // fresh install: nothing to restore
    }
    match crate::memory_bridge::build_memory(&cfg, &state.data_dir) {
        Ok(built) => {
            let arc = std::sync::Arc::new(built);
            let mut slot = state.memory.write().await;
            // Another request may have won the race; either instance is equivalent.
            if slot.is_none() {
                *slot = Some(arc.clone());
            }
            if let Ok(mut err) = state.config_error.lock() {
                *err = None;
            }
            slot.clone()
        }
        Err(e) => {
            if let Ok(mut slot) = state.config_error.lock() {
                *slot = Some(e.to_string());
            }
            // Portable-app rule: an unusable config must not take the process down —
            // /health and /setup have to stay reachable so the credentials can be fixed.
            eprintln!(
                "mem0 is running without a usable runtime: {e}. Configure a provider to \
                 enable memory operations."
            );
            None
        }
    }
}

pub async fn get_config(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let current = load_persisted_config(&state);
    Ok(Json(redact_config(&current, None)))
}

pub async fn list_providers(Extension(_auth): Extension<AuthContext>) -> Json<Value> {
    Json(json!({
        "llm": BUNDLED_LLM_PROVIDERS,
        "embedder": BUNDLED_EMBEDDER_PROVIDERS,
    }))
}

fn validate_bundled_providers(update: &Value) -> ApiResult<()> {
    for (section, allowed) in [
        ("llm", BUNDLED_LLM_PROVIDERS),
        ("embedder", BUNDLED_EMBEDDER_PROVIDERS),
        ("systemone", BUNDLED_SYSTEMONE_PROVIDERS),
    ] {
        if let Some(provider) = update
            .get(section)
            .and_then(|s| s.get("provider"))
            .and_then(|v| v.as_str())
        {
            // An empty provider means "this section is off" — the Configuration page sends
            // it when the operator turns SystemOne back off. For llm/embedder the runtime
            // build rejects it later with its own message, so nothing slips through.
            if provider.is_empty() {
                continue;
            }
            if !allowed.contains(&provider) {
                return Err(ApiError::bad_request(format!(
                    "Unsupported {section} provider '{provider}'. Supported: {}",
                    allowed.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Refuse a vector-size change while stored memories still use the old size
/// (`_validate_embedding_dimensions` in server/main.py).
async fn validate_embedding_dimensions(state: &AppState, updates: &Value) -> ApiResult<()> {
    let requested = updates
        .get("vector_store")
        .and_then(|v| v.get("config"))
        .and_then(|c| c.get("embedding_model_dims"))
        .and_then(|v| v.as_u64());
    let Some(requested) = requested else {
        return Ok(());
    };
    let current = load_persisted_config(state);
    let current_dims = current
        .get("vector_store")
        .and_then(|v| v.get("config"))
        .and_then(|c| c.get("embedding_model_dims"))
        .and_then(|v| v.as_u64());
    let (Some(current_dims), true) = (current_dims, Some(requested) != current_dims) else {
        return Ok(());
    };
    // "Do any memories exist" is a store question. Counting rows with a plain SQLite
    // connection does not work — `mem0` is a vec0 virtual table, so the count fails and
    // the failure used to read as "empty", which let a width change through with data
    // present. The store knows how to answer it.
    let has_rows = match state.memory.read().await.clone() {
        Some(mem) => mem
            .vector_store()
            .list(&Default::default(), Some(1))
            .await
            .map(|hits| !hits.is_empty())
            .unwrap_or(false),
        None => false, // no runtime yet: nothing was ever written
    };
    if has_rows {
        return Err(ApiError::bad_request(format!(
            "Cannot change embedding_model_dims from {current_dims} to {requested} while memories exist. \
             The vector column has a fixed width. POST /reset first, then reconfigure."
        )));
    }
    Ok(())
}

pub async fn set_config(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(update): Json<Value>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    validate_bundled_providers(&update)?;
    validate_embedding_dimensions(&state, &update).await?;

    let previous = load_persisted_config(&state);
    let next = crate::memory_bridge::compute_updated_config(&previous, update);

    // Build the new runtime BEFORE touching the old one: a failed build must leave the
    // working config intact (server_state.py::update_config semantics).
    let new_memory = crate::memory_bridge::build_memory(&next, &state.data_dir)
        .map_err(|e| ApiError::bad_request(format!("Configuration rejected: {e}")))?;

    {
        let mut slot = state.memory.write().await;
        *slot = Some(Arc::new(new_memory));
    }
    // The category classifier follows the systemone section of the new config. A
    // broken systemone build rejects the whole update, same as a broken llm build —
    // a half-applied config would classify with a stale model or not at all.
    let classifier = crate::categories::build_classifier(&next)
        .map_err(|e| ApiError::bad_request(format!("Configuration rejected: {e}")))?;
    {
        let mut slot = state.systemone.write().await;
        *slot = classifier.map(Arc::new);
    }
    // A new configuration invalidates the previous failure: without this the Categories
    // page would keep showing an error from a model that is no longer configured.
    crate::http::routes_systemone::note_classify_result(&state, None);
    persist_config(&state, &next)?;
    Ok(Json(json!({ "message": "Configuration set successfully" })))
}

#[derive(Debug, Deserialize)]
pub struct GenerateInstructionsRequest {
    pub use_case: String,
}

pub async fn generate_instructions(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Json(req): Json<GenerateInstructionsRequest>,
) -> ApiResult<Json<Value>> {
    let _mem = state.memory.read().await.clone().ok_or_else(|| {
        ApiError::unavailable(
            "runtime_not_configured: no provider configured yet. Open /setup to configure one.",
        )
    })?;
    let prompt = format!(
        "You are configuring a memory system. Given the use case below, produce two things:\n\
         1. INSTRUCTIONS: A short paragraph of custom instructions telling the memory extraction system \
         what kinds of facts, preferences, and context to prioritize. Be specific to the use case.\n\
         2. TEST_MESSAGE: A single realistic sentence a user in this use case would say, suitable for \
         testing that the memory system works.\n\n\
         Respond in exactly this format (no markdown, no extra text):\n\
         INSTRUCTIONS: <your instructions>\n\
         TEST_MESSAGE: <your test message>\n\n\
         Use case: {}",
        req.use_case
    );
    // Memory keeps its llm private; generate_instructions is a config-time helper, so
    // build a throwaway llm from the persisted config instead of reaching into Memory.
    let cfg = load_persisted_config(&state);
    let llm_cfg = cfg.get("llm").cloned().unwrap_or(Value::Null);
    let provider = llm_cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("mock")
        .to_string();
    let config = llm_cfg.get("config").cloned().unwrap_or(json!({}));
    let llm = mem0::build_llm(&provider, &config).map_err(ApiError::from)?;
    let response = llm
        .generate(
            &[mem0::Message {
                role: "user".into(),
                content: prompt,
                name: None,
            }],
            &mem0::GenerateOptions::default(),
        )
        .await
        .map_err(ApiError::from)?;
    let mut instructions = response.clone();
    let mut test_message = "I like to hike on weekends.".to_string();
    if response.contains("INSTRUCTIONS:") && response.contains("TEST_MESSAGE:") {
        let parts: Vec<&str> = response.split("TEST_MESSAGE:").collect();
        instructions = parts[0].replace("INSTRUCTIONS:", "").trim().to_string();
        test_message = parts[1].trim().to_string();
    }
    Ok(Json(
        json!({ "custom_instructions": instructions, "test_message": test_message }),
    ))
}

// ---------- server settings (port) ----------

/// Port range a desktop app can bind without privileges. 0 means "pick one at launch".
const MIN_PORT: u16 = 1024;
const AUTO_PORT: u16 = 0;

/// `GET /settings/server` — where the app is reachable, and what the next launch will use.
///
/// The two are reported separately on purpose: a configured port that is taken falls back
/// to an ephemeral one, so "configured" and "actually serving" can differ.
pub async fn get_server_settings(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let configured = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?
        .configured_port();
    let bound = state.bound_port.lock().ok().and_then(|p| *p);
    Ok(Json(json!({
        "port": configured,          // what the next launch will try
        "bound_port": bound,         // what this process is serving right now
        "min_port": MIN_PORT,
        "requires_restart": configured.map(|p| Some(p) != bound).unwrap_or(true),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ServerSettingsUpdate {
    /// `null` (or 0) means "let the OS pick a free port".
    pub port: Option<u16>,
}

/// `PUT /settings/server` — persist the port the next launch should bind.
///
/// Nothing is rebound here: the listener already exists, and swapping it under live
/// connections would drop whatever the dashboard is doing. The response says so.
pub async fn set_server_settings(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(update): Json<ServerSettingsUpdate>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let port = match update.port {
        None | Some(AUTO_PORT) => None,
        Some(p) if p < MIN_PORT => {
            return Err(ApiError::bad_request(format!(
                "Port {p} is below {MIN_PORT}. Ports under {MIN_PORT} need root on Linux and macOS."
            )))
        }
        Some(p) => Some(p),
    };
    {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store
            .set_configured_port(port)
            .map_err(|e| ApiError::internal(format!("save port: {e}")))?;
    }
    let bound = state.bound_port.lock().ok().and_then(|p| *p);
    Ok(Json(json!({
        "message": "Saved. Restart the app to use the new port.",
        "port": port,
        "bound_port": bound,
        "requires_restart": port.map(|p| Some(p) != bound).unwrap_or(true),
    })))
}

// ---------- desktop shell preferences ----------

/// `GET /settings/desktop` — start at login / stay in the tray / start hidden.
///
/// `supported` tells the page which of these the current build can honour: the login
/// entry is a Windows registry value, and the tray options need the GUI shell.
pub async fn get_desktop_settings(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let settings = DesktopSettings::load(&state.data_dir);
    let gui = state
        .gui_attached
        .load(std::sync::atomic::Ordering::Relaxed);
    Ok(Json(desktop_settings_json(&settings, gui, None)))
}

#[derive(Debug, Deserialize)]
pub struct DesktopSettingsUpdate {
    pub autostart: Option<bool>,
    pub minimize_to_tray: Option<bool>,
    pub start_silent: Option<bool>,
}

/// `PUT /settings/desktop` — persist the toggles and apply the login entry.
///
/// The login entry is applied here rather than in the shell because the shell has no way
/// to know a setting changed while it is running; the registry is a side effect of the
/// save, and a failure is reported instead of silently pretending it worked.
pub async fn set_desktop_settings(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(update): Json<DesktopSettingsUpdate>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let mut settings = DesktopSettings::load(&state.data_dir);
    // Partial updates: an omitted field keeps its current value, so the page can send
    // only the switch that moved.
    if let Some(v) = update.autostart {
        settings.autostart = v;
    }
    if let Some(v) = update.minimize_to_tray {
        settings.minimize_to_tray = v;
    }
    if let Some(v) = update.start_silent {
        settings.start_silent = v;
    }
    settings
        .save(&state.data_dir)
        .map_err(|e| ApiError::internal(format!("save desktop settings: {e}")))?;

    let mut error = None;
    if AUTOSTART_SUPPORTED {
        let exe = std::env::current_exe()
            .map_err(|e| ApiError::internal(format!("cannot resolve the executable path: {e}")))?;
        if let Err(e) = apply_autostart(&exe, &settings) {
            // The file already holds the operator's intent; report why the login entry
            // did not follow so the page can show it.
            error = Some(e);
        }
    }
    let gui = state
        .gui_attached
        .load(std::sync::atomic::Ordering::Relaxed);
    Ok(Json(desktop_settings_json(&settings, gui, error)))
}

fn desktop_settings_json(
    settings: &DesktopSettings,
    gui_attached: bool,
    error: Option<String>,
) -> Value {
    json!({
        "autostart": settings.autostart,
        "minimize_to_tray": settings.minimize_to_tray,
        "start_silent": settings.start_silent,
        "supported": {
            "autostart": AUTOSTART_SUPPORTED,
            // The tray options only mean something in the GUI shell; headless serves
            // HTTP and has no window to hide.
            "tray": gui_attached,
        },
        "error": error,
    })
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(
            "/configure",
            axum::routing::get(get_config).post(set_config),
        )
        .route("/configure/providers", axum::routing::get(list_providers))
        .route(
            "/generate-instructions",
            axum::routing::post(generate_instructions),
        )
        .route(
            "/settings/server",
            axum::routing::get(get_server_settings).put(set_server_settings),
        )
        .route(
            "/settings/desktop",
            axum::routing::get(get_desktop_settings).put(set_desktop_settings),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys_are_redacted() {
        let cfg =
            json!({"llm": {"provider": "openai", "config": {"api_key": "sk-x", "model": "gpt-5"}}});
        let r = redact_config(&cfg, None);
        assert_eq!(r["llm"]["config"]["api_key"], json!("[redacted]"));
        assert_eq!(r["llm"]["config"]["model"], json!("gpt-5"));
    }

    #[test]
    fn empty_sensitive_value_stays_empty() {
        let cfg = json!({"llm": {"config": {"api_key": ""}}});
        let r = redact_config(&cfg, None);
        assert_eq!(r["llm"]["config"]["api_key"], json!(""));
    }

    #[test]
    fn key_detection_handles_spellings() {
        assert!(is_sensitive_key("api_key"));
        assert!(is_sensitive_key("apiKey"));
        assert!(is_sensitive_key("OPENAI_API_KEY"));
        assert!(is_sensitive_key("client_secret"));
        assert!(is_sensitive_key("password"));
        assert!(!is_sensitive_key("model"));
        assert!(!is_sensitive_key("base_url"));
    }
}
