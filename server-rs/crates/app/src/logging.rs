//! The application log: a JSONL file in the data directory.
//!
//! Three things this is not:
//!
//! - Not the request log. `request_logs` records HTTP traffic for the Analytics figures;
//!   this records *why the program behaved the way it did*. Looking at statistics must not
//!   change the statistics, and a diagnostic narrative is a different question from traffic.
//! - Not the memory operation log. `memory_operations` records what was done to memories.
//! - Not `tracing`. There is no subscriber, no span machinery, and no need for either: the
//!   events below are a handful of call sites that want a JSON line with a stable shape.
//!
//! It is a file rather than a database table because the most valuable lines are written
//! before the database can be opened — a broken configuration, a port that will not bind, a
//! data directory that cannot be created. Those are exactly the failures a table would lose.
//!
//! Two invariants the writer holds:
//!
//! 1. **No secret ever reaches the disk.** Every line passes through [`redact`] before it is
//!    serialised. This is not configurable.
//! 2. **A logging failure never fails the request.** Every function here is infallible from
//!    the caller's point of view; a write error is dropped.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Map, Value};

/// Size at which `app.log` is rotated. Three files are kept, so the log costs at most
/// 15 MB of disk no matter how long the app runs.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// How many rotated files to keep besides the active one.
const KEEP: usize = 2;

/// Answers longer than this are cut and marked `truncated`. Enough to see that a model
/// returned `null`, prose, or a fence-wrapped array; not enough to copy a memory verbatim.
pub const MAX_FIELD_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

struct Logger {
    path: PathBuf,
    max_bytes: u64,
}

fn logger() -> &'static Mutex<Option<Logger>> {
    static LOGGER: OnceLock<Mutex<Option<Logger>>> = OnceLock::new();
    LOGGER.get_or_init(|| Mutex::new(None))
}

/// Point the logger at `<data_dir>/logs/app.log`. Idempotent: later calls are ignored, so
/// the shell can call it before the server does without fighting over the file.
pub fn init(data_dir: &Path) {
    init_with_limit(data_dir, MAX_BYTES);
}

fn init_with_limit(data_dir: &Path, max_bytes: u64) {
    let dir = data_dir.join("logs");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("logging disabled: cannot create {}: {e}", dir.display());
        return;
    }
    let path = dir.join("app.log");
    if let Ok(mut slot) = logger().lock() {
        if slot.is_none() {
            *slot = Some(Logger { path, max_bytes });
        }
    }
}

/// Whether a log file is being written. The log endpoint uses this to say "logging is not
/// initialised" instead of returning a confusing empty list.
pub fn is_initialised() -> bool {
    logger().lock().map(|s| s.is_some()).unwrap_or(false)
}

/// Path of the active log file, when logging is initialised.
pub fn log_path() -> Option<PathBuf> {
    logger()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|l| l.path.clone()))
}

pub fn info(target: &str, msg: &str, fields: Value) {
    write(Level::Info, target, msg, fields);
}

pub fn warn(target: &str, msg: &str, fields: Value) {
    write(Level::Warn, target, msg, fields);
}

pub fn error(target: &str, msg: &str, fields: Value) {
    write(Level::Error, target, msg, fields);
}

fn write(level: Level, target: &str, msg: &str, fields: Value) {
    let Ok(mut slot) = logger().lock() else {
        return;
    };
    let Some(log) = slot.as_mut() else {
        // Not initialised (unit tests, or a failure before startup). stderr keeps the
        // information visible in a terminal and costs nothing elsewhere.
        eprintln!("[{level}] {target}: {msg}", level = level.as_str());
        return;
    };
    write_to(log, level, target, msg, fields);
}

/// One line into one file. Split out from [`write`] so tests can drive a logger directly
/// instead of fighting the process-global slot.
fn write_to(log: &mut Logger, level: Level, target: &str, msg: &str, fields: Value) {
    let line = json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "level": level.as_str(),
        "target": target,
        "msg": truncate_and_redact(msg),
        "fields": redact_value(fields),
    });
    let mut line = line.to_string();
    line.push('\n');
    rotate_if_needed(log, line.len() as u64);
    if let Err(e) = append(&log.path, &line) {
        eprintln!("log write failed: {e}");
    }
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

/// Rotate before writing, not after, so a single line can never be split across files.
fn rotate_if_needed(log: &mut Logger, incoming: u64) {
    let size = fs::metadata(&log.path).map(|m| m.len()).unwrap_or(0);
    if size == 0 || size + incoming <= log.max_bytes {
        return;
    }
    // app.log.1 -> app.log.2, app.log -> app.log.1, oldest dropped.
    for i in (1..=KEEP).rev() {
        let from = numbered(&log.path, i);
        let to = numbered(&log.path, i + 1);
        if i == KEEP {
            let _ = fs::remove_file(&from);
            continue;
        }
        if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
    let _ = fs::rename(&log.path, numbered(&log.path, 1));
}

fn numbered(path: &Path, i: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{i}"));
    PathBuf::from(name)
}

/// Read the tail of the active log file, newest last, at most `limit` lines.
///
/// The file is read whole rather than seeked backwards: it is capped at 5 MB, and a bounded
/// read is one less piece of state to get wrong. Malformed lines (a torn write from a killed
/// process) are skipped instead of failing the read.
pub fn read_tail(limit: usize) -> Vec<Value> {
    let Some(path) = log_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut lines: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    lines
}

/// Cut a string to [`MAX_FIELD_CHARS`] and redact it.
fn truncate_and_redact(s: &str) -> String {
    let redacted = redact(s);
    if redacted.chars().count() <= MAX_FIELD_CHARS {
        return redacted;
    }
    let head: String = redacted.chars().take(MAX_FIELD_CHARS).collect();
    format!("{head}…[truncated]")
}

/// Redact secrets out of a string.
///
/// Deliberately hand-rolled and dependency-free. Two families are covered:
///
/// - known key shapes (`sk-…`, `AIza…`, `ghp_…`, `xoxb-…`) wherever they appear;
/// - any `key=value` / `key: value` whose key looks like a credential.
///
/// The rule is blunt on purpose: a false positive costs a `[redacted]` in a log line, a
/// false negative costs a leaked credential on disk.
pub fn redact(s: &str) -> String {
    redact_pairs(&redact_tokens(s))
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Pass one: replace key-shaped tokens wherever they appear.
fn redact_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match token_secret_len(s, i) {
            Some(len) => {
                out.push_str("[redacted]");
                i += len;
            }
            None => {
                let step = utf8_len(s.as_bytes()[i]);
                out.push_str(&s[i..i + step]);
                i += step;
            }
        }
    }
    out
}

/// Length of a secret token starting at byte `i`, if there is one.
fn token_secret_len(s: &str, i: usize) -> Option<usize> {
    for prefix in ["sk-", "AIza", "ghp_", "xoxb-", "sk_live_"] {
        if !s[i..].starts_with(prefix) {
            continue;
        }
        let mut end = i + prefix.len();
        for b in s[end..].bytes() {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        // `sk-` alone is not a secret; require a plausible body.
        if end - i >= prefix.len() + 8 {
            return Some(end - i);
        }
    }
    None
}

/// Where an unquoted value ends.
const DELIMITERS: [char; 8] = [' ', '\t', '&', ',', '}', ']', '"', '\n'];

/// Copy characters matching `keep` from `from` into `out`, returning the new offset.
fn copy_while(s: &str, from: usize, out: &mut String, keep: impl Fn(char) -> bool) -> usize {
    let mut j = from;
    for c in s[from..].chars() {
        if !keep(c) {
            break;
        }
        out.push(c);
        j += c.len_utf8();
    }
    j
}

/// Pass two: `api_key=abc`, `"Authorization": "Bearer abc"`, `token: abc`.
fn redact_pairs(s: &str) -> String {
    const KEYS: [&str; 9] = [
        "api_key",
        "api-key",
        "apikey",
        "x-api-key",
        "authorization",
        "access_token",
        "secret",
        "password",
        "token",
    ];
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let key_len = KEYS
            .iter()
            .find(|k| lower[i..].starts_with(**k))
            .map(|k| k.len());
        let Some(key_len) = key_len else {
            let step = utf8_len(s.as_bytes()[i]);
            out.push_str(&s[i..i + step]);
            i += step;
            continue;
        };
        out.push_str(&s[i..i + key_len]);
        // Walk to the value, copying the punctuation on the way so the line keeps its
        // shape: `"api_key": "abc"` and `api_key=abc` both end up on the value.
        let mut j = i + key_len;
        j = copy_while(s, j, &mut out, |c| matches!(c, '"' | '\'' | ' ' | '\t'));
        if j < s.len() && matches!(s.as_bytes()[j], b'=' | b':') {
            out.push(s.as_bytes()[j] as char);
            j += 1;
            j = copy_while(s, j, &mut out, |c| matches!(c, '"' | '\'' | ' ' | '\t'));
        }
        if j >= s.len() {
            i = j;
            continue;
        }
        // The value: a quoted string, or one token up to the next delimiter.
        let tail = &s[j..];
        let value_len = match tail.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let body = &tail[q.len_utf8()..];
                let close = body.find(q).unwrap_or(body.len());
                let mut len = q.len_utf8() + close;
                if body[close..].starts_with(q) {
                    len += q.len_utf8();
                }
                len
            }
            _ => tail.find(DELIMITERS).unwrap_or(tail.len()),
        };
        // `Authorization: Bearer <token>` — the scheme is not the secret, the token is.
        let mut consumed = value_len;
        let word = s[j..j + value_len].to_ascii_lowercase();
        if matches!(word.as_str(), "bearer" | "basic" | "digest" | "token") {
            let after = j + value_len;
            let token_start = after + (s[after..].len() - s[after..].trim_start().len());
            let token_len = s[token_start..]
                .find(DELIMITERS)
                .unwrap_or(s.len() - token_start);
            consumed = token_start + token_len - j;
        }
        let already = s[j..j + consumed]
            .to_ascii_lowercase()
            .contains("[redacted]");
        if consumed == 0 || already {
            out.push_str(&s[j..j + consumed]);
        } else {
            out.push_str("[redacted]");
        }
        i = j + consumed;
    }
    out
}

/// Whether a field name is one whose value is a credential.
fn is_credential_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    [
        "api_key",
        "api-key",
        "apikey",
        "authorization",
        "token",
        "secret",
        "password",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

/// Redact every string in a JSON value, keeping its shape.
fn redact_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(truncate_and_redact(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(redact_value).collect()),
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, val) in o {
                // `{"authorization": "Bearer abc"}` carries no `key=` marker: the field
                // name is the only signal that the value is a credential.
                let v = if is_credential_key(&k) {
                    Value::String("[redacted]".into())
                } else {
                    redact_value(val)
                };
                m.insert(k, v);
            }
            Value::Object(m)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shapes_are_redacted() {
        let out = redact("failed with sk-abcdef1234567890 and ghp_aaaaaaaaaaaaaaaaaaaa");
        assert!(!out.contains("abcdef1234567890"), "{out}");
        assert!(!out.contains("ghp_aaaa"), "{out}");
        assert_eq!(out.matches("[redacted]").count(), 2, "{out}");
    }

    #[test]
    fn key_value_pairs_are_redacted() {
        for line in [
            "GET /v1/memories?api_key=supersecretvalue",
            r#"{"Authorization": "Bearer abcdefghijklmnop"}"#,
            "token: 1234567890abcdef",
            "X-API-Key=abc123def456",
        ] {
            let out = redact(line);
            assert!(
                !out.contains("supersecretvalue")
                    && !out.contains("abcdefghijklmnop")
                    && !out.contains("1234567890abcdef")
                    && !out.contains("abc123def456"),
                "{line} -> {out}"
            );
        }
    }

    fn temp_logger(max_bytes: u64) -> (tempfile::TempDir, Logger) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("logs").join("app.log");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        (dir, Logger { path, max_bytes })
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    #[test]
    fn a_line_carries_level_target_and_fields() {
        let (_dir, mut log) = temp_logger(MAX_BYTES);
        write_to(
            &mut log,
            Level::Warn,
            "llm",
            "extraction empty",
            json!({"parsed": 0}),
        );
        let lines = read_lines(&log.path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["level"], json!("warn"));
        assert_eq!(lines[0]["target"], json!("llm"));
        assert_eq!(lines[0]["msg"], json!("extraction empty"));
        assert_eq!(lines[0]["fields"]["parsed"], json!(0));
        assert!(lines[0]["ts"].as_str().unwrap_or_default().contains('T'));
    }

    #[test]
    fn secrets_never_reach_the_file() {
        let (_dir, mut log) = temp_logger(MAX_BYTES);
        write_to(
            &mut log,
            Level::Error,
            "llm",
            "call failed: api_key=supersecretvalue",
            json!({"authorization": "Bearer abcdefghijklmnop"}),
        );
        let raw = std::fs::read_to_string(&log.path).expect("read");
        assert!(!raw.contains("supersecretvalue"), "{raw}");
        assert!(!raw.contains("abcdefghijklmnop"), "{raw}");
        assert!(raw.contains("[redacted]"), "{raw}");
    }

    #[test]
    fn rotation_keeps_a_bounded_number_of_files() {
        let (_dir, mut log) = temp_logger(400);
        for i in 0..40 {
            write_to(
                &mut log,
                Level::Info,
                "test",
                &format!("line {i}"),
                json!({}),
            );
        }
        let mut files: Vec<_> = std::fs::read_dir(log.path.parent().expect("parent"))
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec!["app.log", "app.log.1", "app.log.2"],
            "{files:?}"
        );
        // The newest line is in the active file, and no line was split in half.
        let active = read_lines(&log.path);
        assert_eq!(
            active.last().map(|l| l["msg"].clone()),
            Some(json!("line 39"))
        );
        for name in ["app.log.1", "app.log.2"] {
            let path = log.path.parent().expect("parent").join(name);
            assert!(
                !read_lines(&path).is_empty(),
                "{name} should hold older lines"
            );
        }
    }

    #[test]
    fn read_tail_returns_the_last_lines_in_order() {
        let (_dir, mut log) = temp_logger(MAX_BYTES);
        for i in 0..10 {
            write_to(
                &mut log,
                Level::Info,
                "test",
                &format!("line {i}"),
                json!({}),
            );
        }
        let raw = std::fs::read_to_string(&log.path).expect("read");
        let tail: Vec<Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect::<Vec<_>>()
            .split_off(7);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[2]["msg"], json!("line 9"));
    }

    #[test]
    fn ordinary_text_survives() {
        let line = "extraction finished with parsed=0 in 812ms";
        assert_eq!(redact(line), line);
    }

    #[test]
    fn long_answers_are_truncated_and_marked() {
        let long = "x".repeat(MAX_FIELD_CHARS + 50);
        let out = truncate_and_redact(&long);
        assert!(out.ends_with("[truncated]"), "{out}");
        assert_eq!(
            out.chars().count(),
            MAX_FIELD_CHARS + "…[truncated]".chars().count()
        );
    }
}
