//! Desktop-shell preferences: start at login, keep running in the tray, start hidden.
//!
//! These belong to the window, not to mem0, so they live in their own file
//! (`desktop.json` in the data directory) rather than in `config.json`: rewriting the
//! provider configuration must not reset them, and they are readable without the server.
//! The Tauri shell reads the file directly at startup and again when the window is
//! closed, so a toggle takes effect without a restart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Registry value name used for the login entry.
pub const AUTOSTART_VALUE_NAME: &str = "mem0-desktop";
/// `HKCU` login key, relative to `HKEY_CURRENT_USER`. Per-user, so no elevation is
/// needed to write it.
pub const AUTOSTART_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopSettings {
    /// Add a login entry so the app starts with Windows.
    pub autostart: bool,
    /// Closing the window keeps the app alive in the tray instead of quitting.
    pub minimize_to_tray: bool,
    /// Open without showing the window (tray icon only).
    pub start_silent: bool,
}

impl DesktopSettings {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("desktop.json")
    }

    /// Missing or unreadable file means "defaults": a broken preference file must not
    /// keep the app from starting.
    pub fn load(data_dir: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(Self::path(data_dir)) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    /// Write atomically (temp file + rename) so a crash mid-write cannot leave a
    /// half-written preferences file behind.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let target = Self::path(data_dir);
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(&tmp, &target)
    }
}

/// Whether the login entry can be managed on this platform.
pub const AUTOSTART_SUPPORTED: bool = cfg!(windows);

/// The command the login entry runs: this executable, quoted because install paths
/// contain spaces, plus `--silent` when the app should start without showing its window.
pub fn login_command(exe: &Path, silent: bool) -> String {
    let quoted = format!("\"{}\"", exe.display());
    if silent {
        format!("{quoted} --silent")
    } else {
        quoted
    }
}

/// Apply the login entry. Returns the error message on failure so the API can report it
/// instead of pretending the setting took effect.
#[cfg(windows)]
pub fn apply_autostart(exe: &Path, settings: &DesktopSettings) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(AUTOSTART_KEY, KEY_SET_VALUE)
        .map_err(|e| format!("cannot open HKCU\\{AUTOSTART_KEY}: {e}"))?;
    if settings.autostart {
        key.set_value(
            AUTOSTART_VALUE_NAME,
            &login_command(exe, settings.start_silent),
        )
        .map_err(|e| format!("cannot write the login entry: {e}"))
    } else {
        match key.delete_value(AUTOSTART_VALUE_NAME) {
            Ok(()) => Ok(()),
            // Already absent is what the caller asked for.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove the login entry: {e}")),
        }
    }
}

/// Everywhere else there is no login entry to manage, so asking is a no-op.
#[cfg(not(windows))]
pub fn apply_autostart(_exe: &Path, _settings: &DesktopSettings) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_all_off() {
        let d = DesktopSettings::default();
        assert!(!d.autostart && !d.minimize_to_tray && !d.start_silent);
    }

    #[test]
    fn round_trips_through_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let settings = DesktopSettings {
            autostart: true,
            minimize_to_tray: true,
            start_silent: false,
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(DesktopSettings::load(dir.path()), settings);
        // Readable as plain JSON, next to the rest of the data directory.
        let raw = std::fs::read_to_string(DesktopSettings::path(dir.path())).unwrap();
        assert!(raw.contains("\"minimize_to_tray\": true"), "{raw}");
    }

    #[test]
    fn a_corrupt_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(DesktopSettings::path(dir.path()), b"{not json").unwrap();
        assert_eq!(
            DesktopSettings::load(dir.path()),
            DesktopSettings::default()
        );
    }

    #[test]
    fn unknown_keys_from_a_newer_build_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            DesktopSettings::path(dir.path()),
            br#"{"autostart": true, "something_new": 1}"#,
        )
        .unwrap();
        assert!(DesktopSettings::load(dir.path()).autostart);
    }

    #[test]
    fn login_entry_points_at_this_exe_and_adds_silent_only_when_asked() {
        let exe = Path::new(r"C:\Program Files\mem0-desktop\mem0-desktop.exe");
        // Quoted: the install path contains spaces.
        assert_eq!(
            login_command(exe, false),
            r#""C:\Program Files\mem0-desktop\mem0-desktop.exe""#
        );
        assert_eq!(
            login_command(exe, true),
            r#""C:\Program Files\mem0-desktop\mem0-desktop.exe" --silent"#
        );
    }

    #[test]
    fn applying_on_a_platform_without_a_login_entry_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let settings = DesktopSettings {
            autostart: true,
            ..Default::default()
        };
        // Linux/macOS: there is no Run key, and asking for one must not fail the save.
        if !AUTOSTART_SUPPORTED {
            assert!(apply_autostart(dir.path(), &settings).is_ok());
        }
    }

    #[test]
    fn the_login_entry_lives_in_the_per_user_run_key() {
        assert_eq!(
            AUTOSTART_KEY,
            r"Software\Microsoft\Windows\CurrentVersion\Run"
        );
        assert_eq!(AUTOSTART_VALUE_NAME, "mem0-desktop");
    }
}
