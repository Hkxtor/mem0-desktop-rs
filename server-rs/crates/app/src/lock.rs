//! Single-instance lock, mirroring `server/desktop/shell.py`.
//!
//! `<data>/app.lock` is claimed atomically with `create_new` (O_EXCL) and holds
//! `{pid, url, created_at}`. A second instance reads the lock:
//! - owner pid is alive → send it a focus request, then exit;
//! - owner pid is dead (or lock is malformed) → steal the lock and continue.
//!
//! "Owner alive" is enough during the launch window: the lock exists from the moment
//! a launch claims it, but `/health` only answers once the server has bound its port.
//! This is the P1-2 fix from the Python app — a second instance must not start a second
//! server on the same data dir while the first is still starting.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const LOCK_FILENAME: &str = "app.lock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockInfo {
    pub pid: u32,
    pub url: String,
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LockOutcome {
    /// We claimed the lock; we are the first instance.
    Acquired,
    /// Another live instance owns the lock.
    Running(LockInfo),
}

/// Outcome of trying to acquire the lock, with the guard when acquired.
pub enum LockGuard {
    Acquired(LockFile),
    Running(LockInfo),
}

pub struct LockFile {
    path: PathBuf,
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCK_FILENAME)
}

/// Try to claim the single-instance lock in `data_dir`, writing `info` into it.
/// If a live owner holds it, returns `LockOutcome::Running` with the owner's info.
pub fn try_acquire(data_dir: &Path, info: &LockInfo) -> std::io::Result<LockOutcome> {
    let path = lock_path(data_dir);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            serde_json::to_writer(&mut f, info)?;
            f.flush()?;
            Ok(LockOutcome::Acquired)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(owner) = read_lock(&path) {
                if process_alive(owner.pid) {
                    return Ok(LockOutcome::Running(owner));
                }
            }
            // Dead owner or unreadable lock: steal it atomically by rename-then-write.
            let tmp = path.with_extension("lock.tmp");
            {
                let mut f = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp)?;
                serde_json::to_writer(&mut f, info)?;
                f.flush()?;
            }
            match fs::rename(&tmp, &path) {
                Ok(()) => Ok(LockOutcome::Acquired),
                Err(_) => {
                    // Lost the rename race to another stealer; re-read.
                    if let Some(owner) = read_lock(&path) {
                        if process_alive(owner.pid) {
                            let _ = fs::remove_file(&tmp);
                            return Ok(LockOutcome::Running(owner));
                        }
                    }
                    let _ = fs::remove_file(&tmp);
                    Err(std::io::Error::other("lost lock steal race"))
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Acquire and return a guard that removes the lock file on drop.
pub fn acquire(data_dir: &Path, info: &LockInfo) -> std::io::Result<LockGuard> {
    match try_acquire(data_dir, info)? {
        LockOutcome::Acquired => Ok(LockGuard::Acquired(LockFile {
            path: lock_path(data_dir),
        })),
        LockOutcome::Running(owner) => Ok(LockGuard::Running(owner)),
    }
}

/// Read and parse the lock file, if present and well-formed.
pub fn read_lock(path: &Path) -> Option<LockInfo> {
    let mut s = String::new();
    fs::File::open(path).ok()?.read_to_string(&mut s).ok()?;
    serde_json::from_str(&s).ok()
}

/// Is `pid` a live process? POSIX: `kill(pid, 0)`. Windows: OpenProcess.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Safety: signal 0 performs error checking without sending a signal.
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    use std::os::windows::ffi::OsStrExt;
    // OpenProcess with SYNCHRONIZE; if it succeeds the process exists.
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(h: isize) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h == 0 {
        return false;
    }
    unsafe { CloseHandle(h) };
    true
}

#[cfg(not(any(unix, windows)))]
pub fn process_alive(_pid: u32) -> bool {
    true
}

/// Best-effort focus request to a running instance. Returns Ok if the instance
/// answered (or we don't care), Err on connection failure.
pub async fn request_focus(url: &str) -> std::io::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(std::io::Error::other)?;
    let resp = client
        .post(format!("{url}/internal/focus"))
        .send()
        .await
        .map_err(std::io::Error::other)?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "focus endpoint returned {}",
            resp.status()
        )))
    }
}

/// Write a lock file directly (used by tests and by the shell to publish the URL).
pub fn write_lock(data_dir: &Path, info: &LockInfo) -> std::io::Result<()> {
    // Atomic replace (temp + rename) so a concurrent reader never sees a
    // truncated/partial lock file and mistakes it for malformed.
    let path = lock_path(data_dir);
    let tmp = path.with_extension("lock.tmp2");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        serde_json::to_writer(&mut f, info)?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)
}
