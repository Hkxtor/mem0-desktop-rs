//! SQLite state store additions for auth: users carry name/email/last_login_at
//! (server/models.py), which the M3-T3 schema predates — this module extends the
//! schema and adds the auth queries.

use rusqlite::params;

use crate::store::{Result, StateStore, User};

pub const AUTH_SCHEMA: &str = r#"
ALTER TABLE users ADD COLUMN name TEXT NOT NULL DEFAULT '';
"#;

impl StateStore {
    /// Open with the full auth schema (adds name/email/last_login_at columns to users
    /// if an older mem0.db lacks them, idempotent via column-exists check).
    pub fn open_auth(path: &std::path::Path) -> Result<Self> {
        let mut store = Self::open(path)?;
        store.ensure_auth_columns()?;
        Ok(store)
    }

    fn ensure_auth_columns(&mut self) -> Result<()> {
        let cols: Vec<String> = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(users)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (col, ty) in [
            ("name", "TEXT NOT NULL DEFAULT ''"),
            ("email", "TEXT"),
            ("last_login_at", "TEXT"),
        ] {
            if !cols.iter().any(|c| c == col) {
                self.conn
                    .execute(&format!("ALTER TABLE users ADD COLUMN {col} {ty}"), [])?;
            }
        }
        // installs created before request logging existed have the table without it
        let log_cols: Vec<String> = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(request_logs)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !log_cols.is_empty() && !log_cols.iter().any(|c| c == "auth_type") {
            self.conn.execute(
                "ALTER TABLE request_logs ADD COLUMN auth_type TEXT NOT NULL DEFAULT 'none'",
                [],
            )?;
        }
        // unique index on email when present
        let _ = self.conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email ON users(email) WHERE email IS NOT NULL AND email != ''",
            [],
        );
        Ok(())
    }

    pub fn get_user_by_email(&self, email: &str) -> Result<Option<AuthUser>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, username, password_hash, role, created_at, updated_at, name, email, last_login_at FROM users WHERE email=?1",
        )?;
        let row = stmt.query_row(params![email], |r| {
            Ok(AuthUser {
                base: User {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    password_hash: r.get(2)?,
                    role: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                },
                name: r.get(6)?,
                email: r.get(7)?,
                last_login_at: r.get(8)?,
            })
        });
        match row {
            Ok(u) => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn insert_auth_user(&self, u: &AuthUser) -> Result<()> {
        self.conn.execute(
            "INSERT INTO users(id, username, password_hash, role, created_at, updated_at, name, email, last_login_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                u.base.id, u.base.username, u.base.password_hash, u.base.role,
                u.base.created_at, u.base.updated_at, u.name, u.email, u.last_login_at
            ],
        )?;
        Ok(())
    }

    pub fn update_last_login(&self, user_id: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE users SET updated_at=?1, last_login_at=?1 WHERE id=?2",
            params![now, user_id],
        )?;
        Ok(())
    }

    pub fn update_password_hash(&self, user_id: &str, hash: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE users SET password_hash=?1, updated_at=?2 WHERE id=?3",
            params![hash, now, user_id],
        )?;
        Ok(())
    }

    pub fn update_user_name_email(&self, user_id: &str, name: &str, email: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE users SET name=?1, email=?2, updated_at=?3 WHERE id=?4",
            params![name, email, now, user_id],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub base: User,
    pub name: String,
    pub email: Option<String>,
    pub last_login_at: Option<String>,
}
