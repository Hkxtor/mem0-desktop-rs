//! mem0-desktop Tauri shell.
//!
//! The binary in `main.rs` carries the shell; this lib target exists so the crate has a
//! testable surface for the parts that are configuration rather than window code.

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// The tray icon is built in `main.rs` so it can carry a menu and handlers. Tauri also
    /// creates one from `app.trayIcon` in this file — with no menu and no event handler,
    /// which is the icon that looks dead next to the real one. Exactly one of the two may
    /// exist, and it is the code-built one.
    #[test]
    fn the_config_does_not_declare_a_second_tray_icon() {
        let config: Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        assert!(
            config["app"].get("trayIcon").is_none(),
            "app.trayIcon creates a second, non-interactive tray icon; the shell builds \
             its own in main.rs with a menu"
        );
    }
}
