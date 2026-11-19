//! SQLite state store: the portable app's account/keys/logs/settings database.
//!
//! Schema mirrors `server/models.py` table by table (users, api_keys, request_logs,
//! refresh_token_jtis, settings) so a `data/mem0.db` written by the Python app opens
//! identically in the Rust one. WAL + foreign keys on, matching `server/db.py`.

use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    Config(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Json(e) => write!(f, "json: {e}"),
            StoreError::Config(m) => write!(f, "config: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

/// Bring an older database up to the current schema.
///
/// `CREATE TABLE IF NOT EXISTS` does nothing to a table that already exists, so every column
/// added after the first release needs an explicit, idempotent `ALTER` here. Both `open` and
/// the in-memory constructor run it, so a test database and a user's database have the same
/// shape.
fn migrate(conn: &Connection) -> Result<()> {
    let add = |table: &str, column: &str, ty: &str| -> Result<()> {
        let exists: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name=?1"),
            params![column],
            |r| r.get(0),
        )?;
        if exists == 0 {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"), [])?;
        }
        Ok(())
    };
    // Credential type per request, added when the Requests page stopped showing HTTP rows.
    add("request_logs", "auth_type", "TEXT NOT NULL DEFAULT 'none'")?;
    // Why a successful memory operation produced nothing.
    add("memory_operations", "reason", "TEXT")?;
    // The stashed add behind an operation that stored nothing but kept its input.
    add("memory_operations", "failed_add_id", "INTEGER")?;
    // Why the upstream call failed, as a code the dashboard can label. `reason` says which step
    // produced nothing; this says what to do about it.
    add("memory_operations", "error_kind", "TEXT")?;
    // What the operation actually stored or found, abbreviated, so the Requests page can show
    // more than a count when a row is opened: `{"items": [...], "total": n, "truncated": bool}`.
    // NULL when the operation has no per-item detail worth keeping (`IMPORT`, `RESET`).
    add("memory_operations", "result_summary", "TEXT")?;
    // Where the time went, as JSON (`llm_ms` / `embed_ms` / `classify_ms` / `other_ms`). NULL
    // for the paths that do not run inside a timed scope, e.g. the failed-add retry loop.
    add("memory_operations", "stages", "TEXT")?;
    Ok(())
}

/// The optional annotations on a `memory_operations` row.
///
/// Every field is something only some request paths know: `reason` and `error_kind` come from
/// the failure paths that had a provider answer to read, `failed_add_id` from the ones that kept
/// their input, `result_summary` and `stages` from the operations the Requests page can expand.
/// Grouping them keeps a caller that knows none of it to `OpExtras::default()`.
#[derive(Debug, Clone, Default)]
pub struct OpExtras<'a> {
    pub reason: Option<&'a str>,
    pub failed_add_id: Option<i64>,
    pub error_kind: Option<&'a str>,
    /// JSON object, already serialized — see `http::ops::summarize`.
    pub result_summary: Option<&'a str>,
    /// JSON object, already serialized — see `stage_timing::measure`.
    pub stages: Option<&'a str>,
}

/// A text column that holds JSON, parsed for the caller.
///
/// Absent stays `null` rather than becoming an empty array or object: the dashboard tells "this
/// operation has no detail" (IMPORT) from "this row is old enough to predate the column".
fn json_column(raw: Option<String>) -> Value {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

/// The columns every failed-add read selects, so the mapper below matches them by name.
const FAILED_ADD_COLUMNS: &str =
    "SELECT id, reason, user_id, agent_id, run_id, metadata, messages, \
     items, infer, memory_type, expiration_date, attempts, last_error, created_at, \
     last_attempt_at FROM failed_adds";

/// One `failed_adds` row as the dashboard's drawer renders it.
fn failed_add_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let messages: String = r.get("messages")?;
    let items: String = r.get("items")?;
    let metadata: Option<String> = r.get("metadata")?;
    Ok(serde_json::json!({
        "id": r.get::<_, i64>("id")?,
        "reason": r.get::<_, String>("reason")?,
        "user_id": r.get::<_, Option<String>>("user_id")?,
        "agent_id": r.get::<_, Option<String>>("agent_id")?,
        "run_id": r.get::<_, Option<String>>("run_id")?,
        "metadata": metadata.and_then(|m| serde_json::from_str::<Value>(&m).ok()),
        "messages": serde_json::from_str::<Value>(&messages).unwrap_or(Value::Array(vec![])),
        "items": serde_json::from_str::<Value>(&items).unwrap_or(Value::Array(vec![])),
        "infer": r.get::<_, i64>("infer")? != 0,
        "memory_type": r.get::<_, Option<String>>("memory_type")?,
        "expiration_date": r.get::<_, Option<String>>("expiration_date")?,
        "attempts": r.get::<_, i64>("attempts")?,
        "last_error": r.get::<_, Option<String>>("last_error")?,
        "created_at": r.get::<_, String>("created_at")?,
        "last_attempt_at": r.get::<_, String>("last_attempt_at")?,
    }))
}

pub struct StateStore {
    pub conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'user',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS api_keys (
    id TEXT PRIMARY KEY,
    key_prefix TEXT NOT NULL,
    key_hash TEXT NOT NULL,
    label TEXT,
    created_by TEXT NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL,
    last_used_at TEXT,
    is_active INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_api_keys_created_by ON api_keys(created_by);
CREATE TABLE IF NOT EXISTS request_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    status_code INTEGER NOT NULL,
    latency_ms REAL NOT NULL,
    auth_type TEXT NOT NULL DEFAULT 'none',
    user_id TEXT,
    api_key_id TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at);
-- Memory operations (ADD / SEARCH / UPDATE / DELETE / ...), one row per action a
-- client asked for. Separate from request_logs on purpose: request_logs is HTTP
-- traffic (which /configure and /auth/* also produce), this is what happened to
-- the memories themselves, which is what the Requests page shows.
CREATE TABLE IF NOT EXISTS memory_operations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    operation TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    result_count INTEGER,
    status_code INTEGER NOT NULL,
    latency_ms REAL NOT NULL,
    created_at TEXT NOT NULL,
    -- Why a successful call produced nothing. NULL for the ordinary case; the
    -- Requests page shows a badge instead of leaving a bare 0 with no explanation.
    reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_memory_operations_created_at ON memory_operations(created_at);
-- Source inputs: the messages a memory was extracted from. One row per message in
-- the add request, keyed by the memory ids that add produced. Kept out of the memory
-- payload on purpose: export must stay byte-comparable with the official one.
CREATE TABLE IF NOT EXISTS memory_inputs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    role TEXT,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_inputs_memory_id ON memory_inputs(memory_id, seq);
-- Failed adds: an ADD that stored nothing for a reason that may pass (provider error,
-- unparseable answer, entries that could not be embedded). Keeps the material a retry
-- needs — the submitted messages and, when extraction produced them, the entries — so
-- the caller does not have to resubmit. Deleted the moment a retry stores something;
-- bounded by retention and a row cap because it holds memory text.
CREATE TABLE IF NOT EXISTS failed_adds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dedup_key TEXT NOT NULL UNIQUE,
    reason TEXT NOT NULL,
    user_id TEXT,
    agent_id TEXT,
    run_id TEXT,
    metadata TEXT,
    messages TEXT NOT NULL,
    items TEXT NOT NULL DEFAULT '[]',
    -- The add's own options, so a retry replays the same request: `infer: false` stores the
    -- message verbatim and must not silently turn into an extraction on the retry.
    infer INTEGER NOT NULL DEFAULT 1,
    memory_type TEXT,
    expiration_date TEXT,
    attempts INTEGER NOT NULL DEFAULT 1,
    last_error TEXT,
    created_at TEXT NOT NULL,
    last_attempt_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_failed_adds_created_at ON failed_adds(created_at);
CREATE TABLE IF NOT EXISTS refresh_token_jtis (
    jti TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    expires_at TEXT NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
"#;

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ApiKeyRow {
    pub id: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub label: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub is_active: bool,
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// In-memory convenience for tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // auth columns (name/email/last_login_at) are added by open_auth in
        // production; tests that touch them need them too.
        conn.execute_batch(crate::store_auth::AUTH_SCHEMA)?;
        migrate(&conn)?;
        for (col, ty) in [("email", "TEXT"), ("last_login_at", "TEXT")] {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name=?1",
                params![col],
                |r| r.get(0),
            )?;
            if exists == 0 {
                conn.execute(&format!("ALTER TABLE users ADD COLUMN {col} {ty}"), [])?;
            }
        }
        Ok(Self { conn })
    }

    // ---- settings (used for the JWT secret) ----

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key=?1")?;
        let row = stmt.query_row(params![key], |r| r.get(0));
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Port the operator chose for the next launch, if any.
    pub fn configured_port(&self) -> Option<u16> {
        self.get_setting("server_port")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|p| *p >= 1024)
    }

    pub fn set_configured_port(&self, port: Option<u16>) -> Result<()> {
        let value = port.map(|p| p.to_string()).unwrap_or_default();
        self.set_setting("server_port", &value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES(?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn get_or_create_jwt_secret(&self) -> Result<String> {
        if let Some(s) = self.get_setting("jwt_secret")? {
            return Ok(s);
        }
        let mut buf = [0u8; 48];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut buf);
        let secret = hex::encode(buf);
        self.set_setting("jwt_secret", &secret)?;
        Ok(secret)
    }

    // ---- users ----

    pub fn insert_user(&self, user: &User) -> Result<()> {
        self.conn.execute(
            "INSERT INTO users(id, username, password_hash, role, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                user.id,
                user.username,
                user.password_hash,
                user.role,
                user.created_at,
                user.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_user_by_id(&self, id: &str) -> Result<Option<User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, username, password_hash, role, created_at, updated_at FROM users WHERE id=?1",
        )?;
        let row = stmt.query_row(params![id], |r| {
            Ok(User {
                id: r.get(0)?,
                username: r.get(1)?,
                password_hash: r.get(2)?,
                role: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        });
        match row {
            Ok(u) => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_user_by_username(&self, username: &str) -> Result<Option<User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, username, password_hash, role, created_at, updated_at FROM users WHERE username=?1",
        )?;
        let row = stmt.query_row(params![username], |r| {
            Ok(User {
                id: r.get(0)?,
                username: r.get(1)?,
                password_hash: r.get(2)?,
                role: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        });
        match row {
            Ok(u) => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// A "real" account is one that can actually sign in — `password_hash` non-empty.
    /// The local-mode `Local` user has an empty hash and must not count as configured
    /// (`server/server_state.py` semantics).
    pub fn has_loginable_user(&self) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM users WHERE password_hash != ''",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Ensure the local-mode owner row exists (idempotent).
    pub fn ensure_local_user(&self) -> Result<()> {
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM users WHERE username='Local'",
            [],
            |r| r.get(0),
        )?;
        if exists > 0 {
            return Ok(());
        }
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        self.insert_user(&User {
            id: id.clone(),
            username: "Local".into(),
            password_hash: String::new(), // empty = cannot log in
            role: "admin".into(),
            created_at: now.clone(),
            updated_at: now,
        })?;
        // Python's local user is User(name="Local", email="") — the dashboard
        // settings page does email.trim() on /auth/me, and null crashes it.
        self.conn.execute(
            "UPDATE users SET name='Local', email='' WHERE id=?1 AND (email IS NULL OR email='')",
            params![id],
        )?;
        Ok(())
    }

    // ---- api keys ----

    pub fn insert_api_key(&self, row: &ApiKeyRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO api_keys(id, key_prefix, key_hash, label, created_by, created_at, last_used_at, is_active)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                row.id, row.key_prefix, row.key_hash, row.label, row.created_by,
                row.created_at, row.last_used_at, row.is_active as i64
            ],
        )?;
        Ok(())
    }

    pub fn list_api_keys(&self) -> Result<Vec<ApiKeyRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, key_prefix, key_hash, label, created_by, created_at, last_used_at, is_active FROM api_keys ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ApiKeyRow {
                id: r.get(0)?,
                key_prefix: r.get(1)?,
                key_hash: r.get(2)?,
                label: r.get(3)?,
                created_by: r.get(4)?,
                created_at: r.get(5)?,
                last_used_at: r.get(6)?,
                is_active: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn find_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, key_prefix, key_hash, label, created_by, created_at, last_used_at, is_active
             FROM api_keys WHERE key_hash=?1 AND is_active=1",
        )?;
        let row = stmt.query_row(params![key_hash], |r| {
            Ok(ApiKeyRow {
                id: r.get(0)?,
                key_prefix: r.get(1)?,
                key_hash: r.get(2)?,
                label: r.get(3)?,
                created_by: r.get(4)?,
                created_at: r.get(5)?,
                last_used_at: r.get(6)?,
                is_active: r.get::<_, i64>(7)? != 0,
            })
        });
        match row {
            Ok(k) => Ok(Some(k)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_api_key(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM api_keys WHERE id=?1", params![id])?;
        Ok(n > 0)
    }

    // ---- request logs ----

    /// Record one served request (`_persist_request_log` in server/main.py).
    ///
    /// Logged from the request middleware, after the response is produced, so the row
    /// reflects what the client actually received.
    pub fn log_request(
        &self,
        method: &str,
        path: &str,
        status: i64,
        latency_ms: f64,
        auth_type: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO request_logs(method, path, status_code, latency_ms, auth_type, created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![method, path, status, latency_ms, auth_type, now],
        )?;
        Ok(())
    }

    /// Record one memory operation. `result_count` is how many memories the
    /// operation produced or touched, when that number exists.
    ///
    /// The trailing arguments are optional annotations that always travel together — why the
    /// result was empty, and which stashed add it points at — so they stay separate rather
    /// than becoming a one-use struct.
    #[allow(clippy::too_many_arguments)]
    pub fn log_memory_operation(
        &self,
        operation: &str,
        detail: &str,
        result_count: Option<i64>,
        status: i64,
        latency_ms: f64,
        reason: Option<&str>,
        failed_add_id: Option<i64>,
    ) -> Result<()> {
        self.log_memory_operation_with_kind(
            operation,
            detail,
            result_count,
            status,
            latency_ms,
            &OpExtras {
                reason,
                failed_add_id,
                ..OpExtras::default()
            },
        )
    }

    /// [`Self::log_memory_operation`], plus what kind of upstream failure caused the row and
    /// what the operation stored or where its time went.
    ///
    /// The extras travel in a struct rather than as more positional arguments: at five of them
    /// a positional list is a guessing game, and most callers have nothing to say about any of
    /// them (`OpExtras::default()`).
    pub fn log_memory_operation_with_kind(
        &self,
        operation: &str,
        detail: &str,
        result_count: Option<i64>,
        status: i64,
        latency_ms: f64,
        extras: &OpExtras<'_>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO memory_operations(operation, detail, result_count, status_code, latency_ms, created_at, reason, failed_add_id, error_kind, result_summary, stages)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                operation,
                detail,
                result_count,
                status,
                latency_ms,
                now,
                extras.reason,
                extras.failed_add_id,
                extras.error_kind,
                extras.result_summary,
                extras.stages
            ],
        )?;
        Ok(())
    }

    // ---- failed adds ----

    /// Keep the material of an add that stored nothing, or merge into the row already
    /// holding the same input.
    ///
    /// `dedup_key` is a hash of the messages and the scope, so a client retrying through an
    /// outage accumulates attempts on one row instead of filling the table with copies.
    /// Returns the row id either way, which is what the Requests row points at.
    #[allow(clippy::too_many_arguments)]
    pub fn record_failed_add(
        &self,
        dedup_key: &str,
        reason: &str,
        scope: (Option<&str>, Option<&str>, Option<&str>),
        metadata: Option<&Value>,
        messages: &[Value],
        items: &[String],
        error: Option<&str>,
        options: (Option<bool>, Option<&str>, Option<&str>),
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let metadata = metadata.map(|m| m.to_string());
        let messages = serde_json::to_string(&Value::Array(messages.to_vec()))?;
        let items = serde_json::to_string(&items.to_vec())?;
        self.conn.execute(
            "INSERT INTO failed_adds(dedup_key, reason, user_id, agent_id, run_id, metadata,
                                     messages, items, infer, memory_type, expiration_date,
                                     attempts, last_error, created_at, last_attempt_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12,?13,?13)
             ON CONFLICT(dedup_key) DO UPDATE SET
                 reason = excluded.reason,
                 attempts = failed_adds.attempts + 1,
                 last_error = excluded.last_error,
                 last_attempt_at = excluded.last_attempt_at,
                 -- A later attempt may have got further than the first: keep whichever
                 -- carries extracted entries. `messages` needs no merge — the key is a hash
                 -- of them, so a conflicting row always carries the same messages.
                 items = CASE WHEN excluded.items = '[]' THEN failed_adds.items ELSE excluded.items END,
                 metadata = COALESCE(excluded.metadata, failed_adds.metadata)",
            params![
                dedup_key,
                reason,
                scope.0,
                scope.1,
                scope.2,
                metadata,
                messages,
                items,
                if options.0.unwrap_or(true) { 1 } else { 0 },
                options.1,
                options.2,
                error,
                now
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM failed_adds WHERE dedup_key = ?1",
            params![dedup_key],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Pending retries, newest first, in the shape the drawer renders.
    pub fn list_failed_adds(&self, limit: i64) -> Result<Vec<Value>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{FAILED_ADD_COLUMNS} ORDER BY id DESC LIMIT ?1"))?;
        let rows = stmt.query_map(params![limit], failed_add_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One pending retry, with everything a retry needs.
    pub fn get_failed_add(&self, id: i64) -> Result<Option<Value>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{FAILED_ADD_COLUMNS} WHERE id = ?1"))?;
        let mut rows = stmt.query_map(params![id], failed_add_row)?;
        Ok(rows.next().transpose()?)
    }

    /// Ids of the oldest pending retries, for a batch.
    pub fn pending_failed_add_ids(&self, limit: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM failed_adds ORDER BY id ASC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_failed_adds(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM failed_adds", [], |r| r.get(0))?)
    }

    /// Forget one pending retry: the retry stored something, or the operator gave up.
    pub fn delete_failed_add(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM failed_adds WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn delete_all_failed_adds(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM failed_adds", [])?)
    }

    /// Trim the retry queue: everything older than `days`, then down to `cap` rows.
    ///
    /// This table holds memory text and grows exactly when the upstream is unstable, so it
    /// is bounded twice — by age and by count. Returns how many rows went.
    pub fn prune_failed_adds(&self, days: i64, cap: i64) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let mut removed = self.conn.execute(
            "DELETE FROM failed_adds WHERE created_at < ?1",
            params![cutoff],
        )?;
        removed += self.conn.execute(
            "DELETE FROM failed_adds WHERE id NOT IN (SELECT id FROM failed_adds ORDER BY id DESC LIMIT ?1)",
            params![cap],
        )?;
        Ok(removed)
    }

    /// Delete request and operation rows older than `days`.
    ///
    /// Fourteen days is the floor the Analytics page depends on: it reads a 12-day window,
    /// so a shorter retention would silently shrink the figures rather than just trim the
    /// history. Returns the number of rows removed from each table.
    pub fn prune_history(&self, days: i64) -> Result<(usize, usize)> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let requests = self.conn.execute(
            "DELETE FROM request_logs WHERE created_at < ?1",
            params![cutoff],
        )?;
        let operations = self.conn.execute(
            "DELETE FROM memory_operations WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok((requests, operations))
    }

    /// Newest first, in the shape the dashboard's Requests page renders.
    ///
    /// `LIST` is filtered out rather than left to age away: the endpoint stopped recording
    /// it, but a store written before that still holds one row per page visit, and those
    /// are exactly the rows that drown the interesting ones.
    pub fn list_memory_operations(&self, limit: i64) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, operation, detail, result_count, status_code, latency_ms, created_at,
                    reason, failed_add_id, error_kind, result_summary, stages
             FROM memory_operations WHERE operation <> 'LIST' ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?.to_string(),
                "operation": r.get::<_, String>(1)?,
                "detail": r.get::<_, String>(2)?,
                "results": r.get::<_, Option<i64>>(3)?,
                "status_code": r.get::<_, i64>(4)?,
                "latency_ms": r.get::<_, f64>(5)?,
                "created_at": r.get::<_, String>(6)?,
                "reason": r.get::<_, Option<String>>(7)?,
                "failed_add_id": r.get::<_, Option<i64>>(8)?,
                "error_kind": r.get::<_, Option<String>>(9)?,
                "result_summary": json_column(r.get::<_, Option<String>>(10)?),
                "stages": json_column(r.get::<_, Option<String>>(11)?),
            }))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Record the messages a memory was extracted from. Replaces any previous rows for
    /// that memory, so re-adding an imported id does not stack duplicates.
    pub fn record_memory_inputs(
        &self,
        memory_id: &str,
        messages: &[(Option<String>, String)],
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        // One transaction per add, not one per memory: replacing the inputs of N memories used to
        // be N separate commits, each with its own fsync, and a crash between them left some
        // memories pointing at the old input and some at the new one.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM memory_inputs WHERE memory_id = ?1",
            params![memory_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO memory_inputs(memory_id, seq, role, content, created_at)
                 VALUES(?1,?2,?3,?4,?5)",
            )?;
            for (seq, (role, content)) in messages.iter().enumerate() {
                stmt.execute(params![memory_id, seq as i64, role, content, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record the source messages for several memories in one transaction.
    ///
    /// An add that produced N memories has one set of inputs, shared by all of them; writing them
    /// one memory at a time paid N commits for a single logical write.
    pub fn record_memory_inputs_for_many(
        &self,
        memory_ids: &[String],
        messages: &[(Option<String>, String)],
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut delete = tx.prepare("DELETE FROM memory_inputs WHERE memory_id = ?1")?;
            let mut insert = tx.prepare(
                "INSERT INTO memory_inputs(memory_id, seq, role, content, created_at)
                 VALUES(?1,?2,?3,?4,?5)",
            )?;
            for memory_id in memory_ids {
                delete.execute(params![memory_id])?;
                for (seq, (role, content)) in messages.iter().enumerate() {
                    insert.execute(params![memory_id, seq as i64, role, content, now])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Oldest first — the order the messages were sent in.
    pub fn list_memory_inputs(&self, memory_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, role, content, created_at FROM memory_inputs
             WHERE memory_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![memory_id], |r| {
            Ok(serde_json::json!({
                "seq": r.get::<_, i64>(0)?,
                "role": r.get::<_, Option<String>>(1)?,
                "content": r.get::<_, String>(2)?,
                "created_at": r.get::<_, String>(3)?,
            }))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn delete_memory_inputs(&self, memory_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM memory_inputs WHERE memory_id = ?1",
            params![memory_id],
        )?;
        Ok(())
    }

    /// Drop the retry queue. Used by a full reset: nothing is pending after one.
    pub fn clear_failed_adds(&self) -> Result<()> {
        self.conn.execute("DELETE FROM failed_adds", [])?;
        Ok(())
    }

    pub fn clear_memory_inputs(&self) -> Result<()> {
        self.conn.execute("DELETE FROM memory_inputs", [])?;
        Ok(())
    }

    pub fn list_request_logs(&self, limit: i64) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, method, path, status_code, latency_ms, auth_type, created_at
             FROM request_logs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            // Field set matches server/models.py::RequestLog; `id` is a string there
            // (UUID), and the dashboard types it as one.
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?.to_string(),
                "method": r.get::<_, String>(1)?,
                "path": r.get::<_, String>(2)?,
                "status_code": r.get::<_, i64>(3)?,
                "latency_ms": r.get::<_, f64>(4)?,
                "auth_type": r.get::<_, String>(5)?,
                "created_at": r.get::<_, String>(6)?,
            }))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Analytics aggregate over request_logs (mirrors routers/analytics.py).
    pub fn analytics_summary(&self, window_days: i64) -> Result<Value> {
        let since = (chrono::Utc::now() - chrono::Duration::days(window_days)).to_rfc3339();
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM request_logs WHERE created_at >= ?1",
            params![since],
            |r| r.get(0),
        )?;
        let ok: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM request_logs WHERE created_at >= ?1 AND status_code < 400",
            params![since],
            |r| r.get(0),
        )?;
        let avg: Option<f64> = self.conn.query_row(
            "SELECT AVG(latency_ms) FROM request_logs WHERE created_at >= ?1",
            params![since],
            |r| r.get(0),
        )?;
        let success_rate = if total > 0 {
            ok as f64 / total as f64
        } else {
            1.0
        };
        // daily buckets
        let mut stmt = self.conn.prepare(
            "SELECT substr(created_at,1,10) AS d, COUNT(*) FROM request_logs
             WHERE created_at >= ?1 GROUP BY d ORDER BY d",
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(serde_json::json!({
                "date": r.get::<_, String>(0)?,
                "count": r.get::<_, i64>(1)?,
            }))
        })?;
        let observed: std::collections::BTreeMap<String, i64> = rows
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|v| {
                Some((
                    v.get("date")?.as_str()?.to_string(),
                    v.get("count")?.as_i64()?,
                ))
            })
            .collect();
        // Days with no traffic are filled in, not omitted: a gap in the chart reads as
        // missing data (Python's _daily_counts pads the series the same way).
        let today = chrono::Utc::now().date_naive();
        let daily: Vec<Value> = (0..window_days.max(1))
            .map(|offset| {
                let date = (today - chrono::Duration::days(window_days.max(1) - 1 - offset))
                    .format("%Y-%m-%d")
                    .to_string();
                let count = observed.get(&date).copied().unwrap_or(0);
                serde_json::json!({ "date": date, "count": count })
            })
            .collect();
        // top_paths mirrors server/routers/analytics.py: busiest endpoints in window.
        let mut stmt = self.conn.prepare(
            "SELECT path, COUNT(*) AS c FROM request_logs
             WHERE created_at >= ?1 GROUP BY path ORDER BY c DESC LIMIT 10",
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(serde_json::json!({
                "path": r.get::<_, String>(0)?,
                "count": r.get::<_, i64>(1)?,
            }))
        })?;
        let top_paths: Vec<Value> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(serde_json::json!({
            "window_days": window_days,
            "operations": total,
            "avg_latency_ms": avg.unwrap_or(0.0),
            "success_rate": success_rate,
            "daily": daily,
            "top_paths": top_paths,
        }))
    }

    // ---- refresh token jtis ----

    pub fn insert_jti(&self, jti: &str, user_id: &str, expires_at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO refresh_token_jtis(jti, user_id, expires_at, revoked) VALUES(?1,?2,?3,0)",
            params![jti, user_id, expires_at],
        )?;
        Ok(())
    }

    /// Atomically mark a refresh token's jti as used, returning whether this call was the
    /// one that consumed it.
    ///
    /// `false` means the jti is unknown, already used, or expired — all three are the same
    /// answer to a caller ("this token is not good"), and collapsing them here is what makes
    /// rotation single-use. Python's `consume_refresh_jti` raises 401 on a missing row for
    /// the same reason: a token whose jti was never registered is not a token this server
    /// issued, and treating "no row" as "not revoked" would let a rotated token be replayed
    /// for its full 30 days.
    pub fn consume_jti(&self, jti: &str) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE refresh_token_jtis SET revoked=1 WHERE jti=?1 AND revoked=0",
            params![jti],
        )?;
        Ok(affected == 1)
    }

    pub fn is_jti_revoked(&self, jti: &str) -> Result<bool> {
        let row: Option<i64> = self
            .conn
            .query_row(
                "SELECT revoked FROM refresh_token_jtis WHERE jti=?1",
                params![jti],
                |r| r.get(0),
            )
            .ok();
        Ok(matches!(row, Some(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    fn stash(s: &StateStore, key: &str) -> i64 {
        s.record_failed_add(
            key,
            "provider_error",
            (Some("u"), None, None),
            None,
            &[serde_json::json!({"role": "user", "content": key})],
            &[],
            Some("429"),
            (Some(true), None, None),
        )
        .unwrap()
    }

    #[test]
    fn recording_inputs_for_many_is_one_atomic_write() {
        let s = StateStore::in_memory().unwrap();
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let messages = vec![
            (Some("user".to_string()), "first".to_string()),
            (Some("assistant".to_string()), "second".to_string()),
        ];
        s.record_memory_inputs_for_many(&ids, &messages).unwrap();
        for id in &ids {
            let rows = s.list_memory_inputs(id).unwrap();
            assert_eq!(rows.len(), 2, "{id} should carry both messages");
            assert_eq!(rows[0]["content"], "first");
            assert_eq!(rows[1]["role"], "assistant");
        }

        // Re-recording replaces rather than appends, and does it for every id.
        s.record_memory_inputs_for_many(&ids, &[(Some("user".to_string()), "new".to_string())])
            .unwrap();
        for id in &ids {
            let rows = s.list_memory_inputs(id).unwrap();
            assert_eq!(
                rows.len(),
                1,
                "{id} should have been replaced, not appended"
            );
            assert_eq!(rows[0]["content"], "new");
        }

        // An empty id list is a no-op, not an error.
        s.record_memory_inputs_for_many(&[], &messages).unwrap();
    }

    #[test]
    fn the_same_failed_add_is_one_row_that_counts_attempts() {
        let s = StateStore::in_memory().unwrap();
        let first = stash(&s, "same");
        let again = stash(&s, "same");
        assert_eq!(first, again, "the same input is the same piece of work");
        assert_eq!(s.count_failed_adds().unwrap(), 1);
        assert_eq!(s.get_failed_add(first).unwrap().unwrap()["attempts"], 2);
    }

    #[test]
    fn the_retry_queue_is_bounded_by_count_as_well_as_age() {
        let s = StateStore::in_memory().unwrap();
        for i in 0..5 {
            stash(&s, &format!("k{i}"));
        }
        // A cap below what is stored evicts the oldest, because the newest are the ones the
        // operator has just been looking at.
        assert_eq!(s.prune_failed_adds(14, 3).unwrap(), 2);
        let rows = s.list_failed_adds(10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["messages"][0]["content"], "k4");
        assert_eq!(rows[2]["messages"][0]["content"], "k2");

        // Age drops the rest of whatever is old.
        s.conn
            .execute(
                "UPDATE failed_adds SET created_at = '2000-01-01T00:00:00+00:00'",
                [],
            )
            .unwrap();
        assert_eq!(s.prune_failed_adds(14, 3).unwrap(), 3);
        assert_eq!(s.count_failed_adds().unwrap(), 0);
    }

    #[test]
    fn a_later_attempt_keeps_the_entries_the_first_one_missing_none_had() {
        let s = StateStore::in_memory().unwrap();
        // First failure had no extracted entries; a later one did. The row must end up with the
        // entries, because they are the only thing that saves a re-extraction.
        let id = s
            .record_failed_add(
                "k",
                "provider_error",
                (Some("u"), None, None),
                None,
                &[serde_json::json!({"role": "user", "content": "hi"})],
                &[],
                Some("down"),
                (Some(true), None, None),
            )
            .unwrap();
        s.record_failed_add(
            "k",
            "embed_failed",
            (Some("u"), None, None),
            None,
            &[serde_json::json!({"role": "user", "content": "hi"})],
            &["I write Rust".to_string()],
            Some("no vectors"),
            (Some(true), None, None),
        )
        .unwrap();
        let row = s.get_failed_add(id).unwrap().unwrap();
        assert_eq!(row["reason"], "embed_failed");
        assert_eq!(row["items"][0], "I write Rust");
        assert_eq!(row["attempts"], 2);
    }

    #[test]
    fn schema_creates_and_local_user_is_not_loginable() {
        let s = StateStore::in_memory().unwrap();
        assert!(!s.has_loginable_user().unwrap());
        s.ensure_local_user().unwrap();
        // Local user exists but has empty hash -> still not loginable
        assert!(!s.has_loginable_user().unwrap());
        let u = s.get_user_by_username("Local").unwrap().unwrap();
        assert_eq!(u.role, "admin");
        // idempotent
        s.ensure_local_user().unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM users WHERE username='Local'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn jwt_secret_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem0.db");
        let s1 = StateStore::open(&path).unwrap();
        let secret1 = s1.get_or_create_jwt_secret().unwrap();
        drop(s1);
        let s2 = StateStore::open(&path).unwrap();
        assert_eq!(s2.get_or_create_jwt_secret().unwrap(), secret1);
    }

    #[test]
    fn api_key_roundtrip_and_hash_lookup() {
        let s = StateStore::in_memory().unwrap();
        s.ensure_local_user().unwrap();
        let local = s.get_user_by_username("Local").unwrap().unwrap();
        let row = ApiKeyRow {
            id: uuid::Uuid::new_v4().to_string(),
            key_prefix: "m0sk_ab12".into(),
            key_hash: hex::encode(sha2::Sha256::digest(b"secret-key-bytes")),
            label: Some("test".into()),
            created_by: local.id.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            last_used_at: None,
            is_active: true,
        };
        s.insert_api_key(&row).unwrap();
        let found = s.find_api_key_by_hash(&row.key_hash).unwrap().unwrap();
        assert_eq!(found.id, row.id);
        assert_eq!(s.list_api_keys().unwrap().len(), 1);
        assert!(s.delete_api_key(&row.id).unwrap());
        assert!(s.find_api_key_by_hash(&row.key_hash).unwrap().is_none());
    }

    #[test]
    fn request_log_and_analytics() {
        let s = StateStore::in_memory().unwrap();
        s.log_request("GET", "/memories", 200, 12.5, "api_key")
            .unwrap();
        s.log_request("POST", "/search", 500, 30.0, "bearer")
            .unwrap();
        let logs = s.list_request_logs(10).unwrap();
        assert_eq!(logs.len(), 2);
        // Newest first, in the shape server/models.py::RequestLog has.
        assert_eq!(logs[0]["path"], serde_json::json!("/search"));
        assert_eq!(logs[0]["auth_type"], serde_json::json!("bearer"));
        assert_eq!(logs[1]["auth_type"], serde_json::json!("api_key"));
        assert!(
            logs[0]["id"].is_string(),
            "the dashboard types id as a string"
        );
        let summary = s.analytics_summary(30).unwrap();
        assert_eq!(summary["operations"], 2);
        assert_eq!(summary["success_rate"], 0.5);
    }
}
