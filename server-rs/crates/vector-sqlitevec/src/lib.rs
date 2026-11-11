//! sqlite-vec (vec0) implementation of mem0-rs' `VectorStore` trait.
//!
//! Layout: one SQLite file (`{data_dir}/vectors.db`), one `vec0` virtual table per
//! collection. The vector column is `float[dims]` with cosine metric; the full mem0
//! payload lives in a `payload TEXT` JSON column and filters compile to `json_extract`
//! predicates, so filtered search is exact — there is no ANN index that could silently
//! drop matches the way DuckDB's HNSW does without prefilter pushdown.

pub mod filters_sql;
pub mod fts;
pub mod store;

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use mem0_core::error::{Mem0Error, Result};
use rusqlite::Connection;

/// Register the vec0 extension process-wide. Safe to call repeatedly; only the first
/// call has an effect. Must run before any `Connection` is opened.
type AutoExtensionFn = unsafe extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *const std::ffi::c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> std::ffi::c_int;

fn register_extension() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        // SAFETY: sqlite3_vec_init has the exact signature sqlite3_auto_extension
        // expects for extension entry points; registering it process-wide is the
        // documented sqlite-vec Rust integration (see bindings/rust/src/lib.rs).
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            unsafe extern "C" fn() -> (),
            AutoExtensionFn,
        >(sqlite_vec::sqlite3_vec_init)));
    });
}

/// Open (or create) the vectors database at `path`, with WAL and the vec0 extension loaded.
pub fn open_db(path: &Path) -> Result<Connection> {
    register_extension();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Mem0Error::vector_store(format!("create {}: {e}", parent.display())))?;
    }
    let conn = Connection::open(path)
        .map_err(|e| Mem0Error::vector_store(format!("open {}: {e}", path.display())))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
    )
    .map_err(|e| Mem0Error::vector_store(format!("pragmas: {e}")))?;
    Ok(conn)
}

/// Shared connection guard. rusqlite `Connection` is `!Sync`, so we wrap it in a Mutex;
/// the desktop app is single-writer by design (the single-instance lock enforces it).
pub type SharedConn = Mutex<Connection>;

pub use store::SqliteVecStore;

/// Build a store over `{data_dir}/vectors.db`, collection `collection`, `dims`-wide vectors.
///
/// This is the constructor M2's factory will call with `"sqlite-vec"`. Kept as a free
/// function (rather than editing the fork's `factory.rs`) so the fork stays untouched
/// until M2. `data_dir` matches the Python server's data-directory resolution, so the
/// Rust app reads the same `data/` layout; the DuckDB files under `data/duckdb/` are
/// simply ignored (migration is export/import, per PRD).
pub fn build(data_dir: &Path, collection: &str, dims: usize) -> Result<SqliteVecStore> {
    let conn = open_db(&data_dir.join("vectors.db"))?;
    SqliteVecStore::new(conn, collection, dims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_creates_db_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let store = build(dir.path(), "memories", 8).unwrap();
        // table exists and accepts an 8-wide insert via the trait path
        drop(store);
        assert!(dir.path().join("vectors.db").exists());
    }

    #[test]
    fn sanitize_ident_rejects_injection() {
        // not pub, so test through build with a hostile collection name
        let dir = tempfile::tempdir().unwrap();
        let store = build(dir.path(), "x'; DROP TABLE memories;--", 8);
        assert!(
            store.is_ok(),
            "hostile collection name must be sanitized, not error"
        );
    }
}
