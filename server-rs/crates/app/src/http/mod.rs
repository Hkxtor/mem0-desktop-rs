//! HTTP layer: router assembly, shared state, error type.

pub mod auth;
pub mod embedded;
pub mod error;
pub mod ops;
pub mod request_log;
pub mod routes_auth;
pub mod routes_config;
pub mod routes_diagnostics;
pub mod routes_health;
pub mod routes_logs;
pub mod routes_memories;
pub mod routes_misc;
pub mod routes_systemone;
pub mod static_files;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;

use crate::store::StateStore;
pub use crate::store::StateStore as StateStorePub;

/// Shared application state, handed to every handler via `axum::extract::State`.
/// Process-wide focus bus. `/internal/focus` sends on it; the Tauri shell subscribes
/// to bring its window to front. Headless mode simply never subscribes.
static FOCUS_BUS: std::sync::OnceLock<tokio::sync::broadcast::Sender<()>> =
    std::sync::OnceLock::new();
static CLOSE_BUS: std::sync::OnceLock<tokio::sync::broadcast::Sender<()>> =
    std::sync::OnceLock::new();
static GUI_ATTACHED: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

/// Whether the GUI shell has finished starting (window + tray exist).
///
/// Process-global because the shell only receives the listening address from
/// [`crate::start_server`]; it never sees the `AppState`. Headless runs never set it.
pub fn gui_flag() -> &'static Arc<std::sync::atomic::AtomicBool> {
    GUI_ATTACHED.get_or_init(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
}

/// Called by the desktop shell once its window and tray exist.
pub fn mark_gui_attached() {
    gui_flag().store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn focus_bus() -> &'static tokio::sync::broadcast::Sender<()> {
    FOCUS_BUS.get_or_init(|| tokio::sync::broadcast::channel(16).0)
}

/// Window-close requests, delivered to the GUI shell the same way focus requests are.
///
/// The window's close button and `POST /internal/close-window` end up in the same
/// handler, so the shell's close-to-tray decision can be exercised without a mouse.
pub fn close_bus() -> &'static tokio::sync::broadcast::Sender<()> {
    CLOSE_BUS.get_or_init(|| tokio::sync::broadcast::channel(16).0)
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub store: Arc<Mutex<StateStore>>,
    /// GUI shell subscribes via [`focus_bus`] to get focus requests from a second
    /// instance. Headless mode just never drains the receiver.
    pub focus_tx: tokio::sync::broadcast::Sender<()>,
    /// The live mem0 `Memory`, once `/configure` has succeeded. `None` before that
    /// (the fresh-install state that answers 503 with a /setup hint).
    pub memory: Arc<tokio::sync::RwLock<Option<Arc<mem0::Memory>>>>,
    /// The category classifier, once a `systemone` section is configured. `None` means
    /// classification is off (or not yet built lazily from the persisted config).
    pub systemone: Arc<tokio::sync::RwLock<Option<Arc<crate::categories::Classifier>>>>,
    /// Why the last classification attempt failed, when one did. Classification is
    /// best-effort, so a wrong base_url or a rejected key otherwise looks exactly like
    /// "no categories apply" — an empty `categories` array. The Categories page and
    /// `/systemone/counts` surface this instead.
    pub systemone_error: Arc<std::sync::Mutex<Option<String>>>,
    /// Why the last attempt to build the runtime failed, when one did. Lets the 503
    /// name the real cause (bad provider name, rejected key) instead of claiming
    /// nothing was configured — mirrors `_initialization_error` in `server_state.py`.
    pub config_error: Arc<std::sync::Mutex<Option<String>>>,
    /// The port this process actually bound. `/settings/server` reports it so the
    /// Configuration page can show where the app is reachable right now, which is not
    /// always the configured number (the configured one may be taken).
    pub bound_port: Arc<std::sync::Mutex<Option<u16>>>,
    /// Set by the GUI shell once its window and tray exist. `/health` and
    /// `/settings/desktop` report it so the dashboard can tell a desktop launch from a
    /// headless one, where tray behaviour has nothing to act on.
    pub gui_attached: Arc<std::sync::atomic::AtomicBool>,
    pub auth_disabled: bool,
    pub jwt_secret: String,
    pub rate_limiter: auth::RateLimiter,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> crate::store::Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let focus_tx = focus_bus().clone();
        let store = StateStore::open_auth(&data_dir.join("mem0.db"))?;
        let auth_disabled = matches!(
            std::env::var("AUTH_DISABLED").as_deref(),
            Ok("1") | Ok("true") | Ok("yes") | Ok("on")
        );
        let jwt_secret = match std::env::var("JWT_SECRET") {
            Ok(s) if !s.is_empty() => s,
            _ => store.get_or_create_jwt_secret()?,
        };
        Ok(Self {
            focus_tx,
            data_dir,
            store: Arc::new(Mutex::new(store)),
            memory: Arc::new(tokio::sync::RwLock::new(None)),
            systemone: Arc::new(tokio::sync::RwLock::new(None)),
            systemone_error: Arc::new(std::sync::Mutex::new(None)),
            config_error: Arc::new(std::sync::Mutex::new(None)),
            bound_port: Arc::new(std::sync::Mutex::new(None)),
            gui_attached: gui_flag().clone(),
            auth_disabled,
            jwt_secret,
            rate_limiter: auth::RateLimiter::new(),
        })
    }
}

/// The live category classifier, lazily built from the persisted config on first use
/// (mirrors `memory_or_restore`): `None` when no `systemone` section is configured.
pub async fn classifier_or_restore(
    state: &Arc<AppState>,
) -> Option<Arc<crate::categories::Classifier>> {
    if let Some(c) = state.systemone.read().await.clone() {
        return Some(c);
    }
    let cfg = routes_config::load_persisted_config(state);
    if !crate::categories::is_configured(&cfg) {
        return None;
    }
    match crate::categories::build_classifier(&cfg) {
        Ok(Some(c)) => {
            let c = Arc::new(c);
            let mut slot = state.systemone.write().await;
            *slot = Some(c.clone());
            Some(c)
        }
        _ => None,
    }
}

/// Assemble the full router. Submodules register their routes here; static dashboard
/// and fallback land in M3-T9.
pub fn build_router(state: AppState) -> Router {
    build_router_arc(Arc::new(state))
}

/// Same as [`build_router`], for callers that need the `Arc<AppState>` back — the
/// startup path builds the runtime on the state before the router takes ownership.
pub fn build_router_arc(state: Arc<AppState>) -> Router {
    let static_r = static_files::static_routes(&state);
    let public = Router::new()
        .route("/health", axum::routing::get(routes_health::health))
        .route("/internal/focus", axum::routing::post(routes_health::focus))
        .route(
            "/internal/close-window",
            axum::routing::post(routes_health::close_window),
        )
        .route(
            "/docs/oauth2-redirect",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<!doctype html><html><title>Swagger UI: OAuth2 Redirect</title></html>",
                )
            }),
        );
    let protected = routes_memories::routes()
        .merge(routes_config::routes())
        .merge(routes_auth::routes())
        .merge(routes_misc::routes())
        .merge(routes_systemone::routes())
        .route(
            "/diagnostics/storage",
            axum::routing::get(routes_diagnostics::storage_diagnostics),
        )
        .route("/logs/app", axum::routing::get(routes_logs::app_log))
        .route(
            "/logs/requests",
            axum::routing::get(routes_logs::request_log),
        );
    public
        .merge(protected)
        .merge(static_r)
        .layer(axum::middleware::from_fn(
            static_files::security_headers_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        // Outermost: it must see the response of every layer below, including the ones
        // that reject a request. It drops the slot `auth_middleware` records into.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_log::log_requests,
        ))
        .with_state(state)
}
