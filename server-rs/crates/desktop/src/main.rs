//! mem0-desktop Tauri 2 shell (M4).
//!
//! One process = axum server (in-process tokio task) + WebView window + tray.
//! The HTTP API stays on loopback (dashboard zero-change); the window just loads it.
//!
//! Lifecycle, mirroring `server/desktop/shell.py`:
//! 1. resolve data dir (`MEM0_DATA_DIR` else `<exe>/data`)
//! 2. claim the single-instance lock; if a live owner exists, ask it to focus and exit
//! 3. start the server on the preferred port or an ephemeral one
//! 4. publish the actual URL into the lock file
//! 5. open the window at that URL (hidden when "start minimized" is on), tray menu
//! 6. closing the window quits the app, unless "keep running in the tray" is on — then
//!    the window hides and Quit from the tray menu is the way out
//!
//! Desktop preferences live in `<data>/desktop.json` and are re-read when the window is
//! closed, so toggling one in the dashboard takes effect without a restart.
//!
//! `--headless` skips all GUI and just serves HTTP (CI / e2e path).

// Windows: GUI-subsystem binary so double-clicking the exe does not open a
// console window. Headless/CI output still works because stdout handles are
// inherited when the process is launched with redirection.
#![cfg_attr(windows, windows_subsystem = "windows")]
use std::path::PathBuf;

use mem0_desktop_rs::desktop_settings::DesktopSettings;
use mem0_desktop_rs::http::focus_bus;
use mem0_desktop_rs::lock::{self, LockInfo};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

/// Wait for Ctrl+C on any platform, plus SIGTERM on unix. On Windows, window
/// close already removes the lock via the RunEvent::ExitRequested handler.
#[cfg(unix)]
async fn wait_for_terminate_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(t) => t,
        Err(_) => {
            tokio::signal::ctrl_c().await.ok();
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_terminate_signal() {
    tokio::signal::ctrl_c().await.ok();
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();

    if mem0_desktop_rs::headless_requested(&args) {
        eprintln!("headless mode: serving HTTP only");
        mem0_desktop_rs::main_entry();
        return;
    }

    // Bootstrap outside the tauri event loop: data dir, lock, server.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (url, data_dir, lock_guard) = rt.block_on(async {
        let dir = mem0_desktop_rs::data_dir();
        std::fs::create_dir_all(&dir).expect("create data dir");

        let pid = std::process::id();
        let probe = LockInfo {
            pid,
            url: String::new(),
            created_at: now_secs(),
        };
        let guard = match lock::acquire(&dir, &probe).expect("acquire lock") {
            g @ lock::LockGuard::Acquired(_) => g,
            lock::LockGuard::Running(owner) => {
                eprintln!(
                    "mem0-desktop is already running at {} (pid {}); asking it to focus",
                    owner.url, owner.pid
                );
                let _ = lock::request_focus(&owner.url).await;
                std::process::exit(0);
            }
        };

        let (handle, addr) = mem0_desktop_rs::start_server(&dir, mem0_desktop_rs::cli_port(&args))
            .await
            .expect("start server");
        // Publish the real URL into the lock so a second instance can focus us.
        let info = LockInfo {
            pid,
            url: format!("http://{addr}/"),
            created_at: now_secs(),
        };
        lock::write_lock(&dir, &info).expect("rewrite lock with url");
        eprintln!(
            "mem0-desktop listening on http://{addr} (data: {})",
            dir.display()
        );
        // The server task must outlive this block_on; forget the handle so the
        // runtime keeps it alive (the process exits via tauri's run loop).
        std::mem::forget(handle);
        // Signal handling lives on OUR runtime (rt), which is definitely polling:
        // on SIGTERM/SIGINT remove the lock synchronously and exit, so no stale
        // lock is left behind even though Drop cannot run on signal death.
        {
            let lock_path = dir.join("app.lock");
            let rt_handle = tokio::runtime::Handle::current();
            rt_handle.spawn(async move {
                wait_for_terminate_signal().await;
                eprintln!("signal received: removing lock and exiting");
                let _ = std::fs::remove_file(&lock_path);
                std::process::exit(0);
            });
        }
        (format!("http://{addr}/"), dir, guard)
    });

    let lock_path = data_dir.join("app.lock");
    // Read once for the initial window state; the close handler re-reads so a toggle
    // applies immediately.
    let prefs_dir = data_dir.clone();
    let start_hidden =
        DesktopSettings::load(&data_dir).start_silent || args.iter().any(|a| a == "--silent");

    tauri::Builder::default()
        .setup(move |app| {
            // Window pointed at the loopback server. With "start minimized" the window
            // is built hidden: the server, tray icon and single-instance lock all come
            // up, but nothing pops on screen at login.
            let _window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse().unwrap()),
            )
            .title("mem0")
            .inner_size(1200.0, 800.0)
            .visible(!start_hidden)
            // F12 / Ctrl+Shift+I opens DevTools in the WebView2. This is a
            // loopback-only single-user local app, so the usual release-mode
            // concern does not apply; it makes field debugging possible.
            .devtools(true)
            .build()?;

            // Tray: Open dashboard / Open data folder / Quit.
            let open = MenuItem::with_id(app, "open", "Open dashboard", true, None::<&str>)?;
            let folder = MenuItem::with_id(app, "folder", "Open data folder", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &folder, &quit])?;
            let data_dir = data_dir.clone();
            // Exactly one tray icon: this one. `app.trayIcon` in tauri.conf.json would
            // add a second without a menu or handlers (it looks like a dead icon next to
            // this one), so that block stays out of the config — see the guard test in
            // src/lib.rs.
            let mut tray = TrayIconBuilder::with_id("main")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "folder" => open_folder(&data_dir),
                    "quit" => app.exit(0),
                    _ => {}
                });
            // A tray without an icon is invisible, but a missing default icon must not
            // take the whole app down with a panic in setup.
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let _tray = tray.build(app)?;

            // Forward focus requests (from a second instance hitting
            // POST /internal/focus) to the window.
            let fh = app.handle().clone();
            let mut rx = focus_bus().subscribe();
            tauri::async_runtime::spawn(async move {
                while rx.recv().await.is_ok() {
                    if let Some(w) = fh.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            });

            // POST /internal/close-window closes the window through the same path as
            // the close button, so close-to-tray can be exercised without a mouse.
            let fh = app.handle().clone();
            let mut close_rx = mem0_desktop_rs::http::close_bus().subscribe();
            tauri::async_runtime::spawn(async move {
                while close_rx.recv().await.is_ok() {
                    if let Some(w) = fh.get_webview_window("main") {
                        eprintln!("closing window on request");
                        if let Err(e) = w.close() {
                            eprintln!("window close failed: {e}");
                        }
                    } else {
                        eprintln!("close requested but there is no window");
                    }
                }
            });

            // Keep the lock guard alive for the app lifetime; its Drop removes the
            // lock file on exit.
            app.manage(LockGuardHolder(lock_guard));

            // From here on the window and tray exist, so /health and the desktop
            // settings can report that tray behaviour is live (and the close endpoint
            // has a receiver).
            mem0_desktop_rs::http::mark_gui_attached();

            // SIGTERM/SIGINT -> orderly exit so the lock file is removed (Drop /
            // ExitRequested handler) instead of leaving a stale lock behind.
            let fh = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                #[cfg(unix)]
                {
                    let mut term =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("SIGTERM handler");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                }
                #[cfg(not(unix))]
                {
                    tokio::signal::ctrl_c().await.ok();
                }
                eprintln!("signal received: exiting");
                fh.exit(0);
            });
            Ok(())
        })
        .on_window_event(move |window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() != "main" {
                    return;
                }
                // Re-read: the toggle may have been flipped since launch.
                let keep_in_tray = DesktopSettings::load(&prefs_dir).minimize_to_tray;
                eprintln!("close requested (keep_in_tray={keep_in_tray})");
                if keep_in_tray {
                    // Hide instead of quitting; Quit in the tray menu is the way out.
                    api.prevent_close();
                    let _ = window.hide();
                    eprintln!("window hidden to tray (close-to-tray is on)");
                } else {
                    // Closing the window quits the whole app (server + lock release).
                    window.app_handle().exit(0);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("build tauri app")
        .run(move |_app_handle, event| match event {
            tauri::RunEvent::ExitRequested { .. } => {
                eprintln!("exit requested: removing lock {}", lock_path.display());
                let _ = std::fs::remove_file(&lock_path);
            }
            tauri::RunEvent::Exit => {
                let _ = std::fs::remove_file(&lock_path);
            }
            _ => {}
        });
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn open_folder(dir: &PathBuf) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

#[allow(dead_code)]
struct LockGuardHolder(lock::LockGuard);
