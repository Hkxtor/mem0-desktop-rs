//! mem0-desktop Rust rewrite — HTTP server entry (M3) + server bootstrap (M4).
//!
//! Serves the full API surface on `127.0.0.1` (configurable via `MEM0_PORT`),
//! mirroring `server/main.py`. M4 adds [`start_server`] so the Tauri shell and the
//! headless binary share one bootstrap: bind the preferred port or fall back to an
//! ephemeral one, and hand back the actual address.

pub mod categories;
pub mod config;
pub mod decision;
pub mod desktop_settings;
pub mod http;
pub mod lock;
pub mod logging;
pub mod memory_bridge;
pub mod store;
pub mod store_auth;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use tokio::task::JoinHandle;

/// Resolve the data directory the way `server/config.py::data_dir()` does:
/// `MEM0_DATA_DIR` wins, else `<exe dir>/data`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MEM0_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let exe = std::env::current_exe().ok();
    let base = exe
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("data")
}

/// Which port to try, in precedence order.
///
/// `--port` wins because it is per-launch and explicit; the port saved from the
/// Configuration page comes next, since that is what the operator chose in the UI;
/// `MEM0_PORT` last, so an environment set up before this setting existed keeps working.
fn preferred_port(cli: Option<u16>, saved: Option<u16>) -> Option<u16> {
    cli.or(saved)
        .or_else(|| std::env::var("MEM0_PORT").ok().and_then(|p| p.parse().ok()))
}

/// Start the HTTP server in-process and return the join handle plus the actual
/// bound address. Binds `preferred` first; if it is taken, falls back to an
/// ephemeral port (`:0`) — this removes the probe-then-bind race that
/// `server/desktop/shell.py::pick_port` has.
pub async fn start_server(
    dir: &Path,
    cli_port: Option<u16>,
) -> std::io::Result<(JoinHandle<()>, SocketAddr)> {
    std::fs::create_dir_all(dir)?;
    // First thing after the directory exists: everything below can fail in a way that is
    // otherwise invisible on Windows, where the binary is a GUI-subsystem executable and
    // stderr goes nowhere.
    logging::init(dir);
    logging::info(
        "server",
        "starting",
        serde_json::json!({
            // Not the crate version (0.0.0 for every build): the pid and the binary path
            // are what actually distinguish one launch from another when two instances
            // fight over the lock or the wrong exe is being run.
            "pid": std::process::id(),
            "exe": std::env::current_exe().ok().map(|p| p.display().to_string()),
            "data_dir": dir.display().to_string(),
            "auth_disabled": std::env::var("AUTH_DISABLED").ok(),
            "cli_port": cli_port,
        }),
    );
    // Local-mode default, mirroring server/run.py::prepare_environment: this
    // server only ever binds 127.0.0.1, so authentication is relaxed unless the
    // operator set AUTH_DISABLED explicitly. Without this, a double-clicked
    // desktop launch (no env vars) would 401 every dashboard API call.
    if std::env::var_os("AUTH_DISABLED").is_none() {
        // Safety: called before any threads read the env; single-writer at startup.
        unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    }
    let state = std::sync::Arc::new(
        http::AppState::new(dir.to_path_buf())
            .map_err(|e| std::io::Error::other(format!("init state: {e}")))?,
    );
    // A stored configuration must come back up ready to serve memory requests;
    // otherwise a restart silently degrades to "no provider configured".
    http::routes_config::restore_persisted_runtime(&state).await;
    spawn_history_pruning(state.clone());
    let saved_port = state
        .store
        .lock()
        .ok()
        .and_then(|store| store.configured_port());
    let app = http::build_router_arc(state.clone());

    let preferred = preferred_port(cli_port, saved_port);
    let listener = match preferred {
        Some(p) => match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], p))).await
        {
            Ok(l) => l,
            Err(_) => {
                logging::warn(
                    "server",
                    "configured port in use; using an ephemeral port",
                    serde_json::json!({"requested": p}),
                );
                eprintln!("port {p} is in use; falling back to an ephemeral port");
                tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?
            }
        },
        None => tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?,
    };
    let addr = listener.local_addr()?;
    if let Ok(mut slot) = state.bound_port.lock() {
        *slot = Some(addr.port());
    }
    logging::info(
        "server",
        "listening",
        serde_json::json!({
            "addr": addr.to_string(),
            "requested_port": preferred,
            "saved_port": saved_port,
        }),
    );
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            logging::error(
                "server",
                "serve failed",
                serde_json::json!({"error": e.to_string()}),
            );
            eprintln!("server error: {e}");
        }
    });
    Ok((handle, addr))
}

/// How long the request and operation logs are kept.
///
/// Must stay above the Analytics page's 12-day window, otherwise pruning would make the
/// figures fall off a cliff instead of just dropping old detail.
pub const HISTORY_RETENTION_DAYS: i64 = 14;
const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Trim the two log tables at startup and every six hours after that.
///
/// Both grow without bound otherwise: a dashboard tab left open polls enough endpoints to
/// add thousands of rows a day. Pruning runs off the request path so a slow delete never
/// delays a memory call.
fn spawn_history_pruning(state: std::sync::Arc<http::AppState>) {
    tokio::spawn(async move {
        loop {
            // The guard is not `Send`, so it must not be alive across the sleep: the
            // whole prune happens inside this block.
            let outcome = match state.store.lock() {
                Ok(store) => Some(store.prune_history(HISTORY_RETENTION_DAYS)),
                Err(_) => None,
            };
            match outcome {
                Some(Ok((requests, operations))) if requests + operations > 0 => {
                    logging::info(
                        "store",
                        "pruned log history",
                        serde_json::json!({
                            "retention_days": HISTORY_RETENTION_DAYS,
                            "request_rows": requests,
                            "operation_rows": operations,
                        }),
                    );
                }
                Some(Ok(_)) | None => {}
                Some(Err(e)) => logging::warn(
                    "store",
                    "pruning log history failed",
                    serde_json::json!({"error": e.to_string()}),
                ),
            }
            tokio::time::sleep(PRUNE_INTERVAL).await;
        }
    });
}

/// Headless CLI mode: `--headless` flag or `MEM0_HEADLESS=true`.
pub fn headless_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--headless")
        || std::env::var("MEM0_HEADLESS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
}

/// Parse `--port N` from CLI args (overrides MEM0_PORT).
pub fn cli_port(args: &[String]) -> Option<u16> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--port" {
            if let Some(v) = it.next() {
                return v.parse().ok();
            }
        }
    }
    None
}

#[tokio::main]
async fn main_impl() {
    let args: Vec<String> = std::env::args().collect();
    let dir = data_dir();
    let (_handle, addr) = start_server(&dir, cli_port(&args))
        .await
        .expect("start server");
    println!(
        "mem0-desktop-rs listening on http://{addr} (data: {})",
        dir.display()
    );
    // Keep the process alive; the server task runs until killed.
    tokio::signal::ctrl_c().await.ok();
}

/// Library entry for tests: build the router without binding.
pub fn app_for_test(dir: PathBuf) -> Router {
    // golden files were generated under AUTH_DISABLED=true; tests mirror that.
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let state = http::AppState::new(dir.clone()).expect("init state");
    http::build_router(state)
}

// keep Router import used even before submodules land
#[allow(dead_code)]
fn _t(_: Router) {}

/// Blocking entry called from the binary's `main`.
pub fn main_entry() {
    main_impl();
}

/// Test-only: build a fresh router for `dir` with `memory` already configured.
pub fn app_for_test_with_memory(dir: PathBuf, memory: Arc<mem0::Memory>) -> Router {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let mut state = http::AppState::new(dir.clone()).expect("init state");
    state.memory = Arc::new(tokio::sync::RwLock::new(Some(memory)));
    http::build_router(state)
}

/// Test-only setter used by golden_http's seed helper.
static TEST_MEMORY: std::sync::Mutex<Option<(PathBuf, mem0::Memory)>> = std::sync::Mutex::new(None);
pub fn set_test_memory(dir: PathBuf, memory: mem0::Memory) {
    *TEST_MEMORY.lock().unwrap() = Some((dir, memory));
}
