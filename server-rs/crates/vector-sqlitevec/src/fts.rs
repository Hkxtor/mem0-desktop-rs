//! FTS5 keyword search support for the vec0 store (M5 / R8).
//!
//! vec0 virtual tables expose no `rowid`, so the FTS index is an **independent**
//! FTS5 table keyed by `memory_id` (verified by `examples/fts_spike.rs`), kept in
//! sync by the store's write path (insert/update/delete/delete_col/reset) inside
//! the same transaction. On open, an existing vec0 table without its FTS twin is
//! backfilled once from `json_extract(payload, '$.text_lemmatized')`.
//!
//! Score convention: SQLite's `bm25()` returns *negative* values (better match =
//! more negative). The fork's orchestrator (`mem0-core` `memory.rs`) only counts
//! keyword scores with `raw > 0.0`, matching pgvector's `ts_rank_cd` direction, so
//! we return `-bm25(...)` (positive, better = larger).

use mem0_core::error::{Mem0Error, Result};
use mem0_core::types::{JsonMap, SearchHit};
use rusqlite::Connection;

use crate::filters_sql::compile_filters_v2;

/// Name of the FTS5 twin table for vec0 table `table`.
pub fn fts_table(table: &str) -> String {
    format!("{table}_fts")
}

/// Create the FTS5 table if missing. Idempotent.
pub fn ensure_fts(conn: &Connection, table: &str) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING fts5(memory_id UNINDEXED, text_lemmatized)",
        fts_table(table)
    ))
    .map_err(|e| Mem0Error::vector_store(format!("create fts table: {e}")))?;
    Ok(())
}

/// Backfill the FTS table from existing vec0 rows that are not indexed yet.
/// Uses INSERT ... SELECT with a LEFT JOIN anti-join so re-running is cheap.
pub fn backfill_fts(conn: &Connection, table: &str) -> Result<()> {
    conn.execute_batch(&format!(
        "INSERT INTO {fts}(memory_id, text_lemmatized)
         SELECT v.memory_id, json_extract(v.payload, '$.text_lemmatized')
         FROM {table} v
         LEFT JOIN {fts} f ON f.memory_id = v.memory_id
         WHERE f.memory_id IS NULL AND json_extract(v.payload, '$.text_lemmatized') IS NOT NULL",
        fts = fts_table(table),
        table = table,
    ))
    .map_err(|e| Mem0Error::vector_store(format!("backfill fts: {e}")))?;
    Ok(())
}

/// Upsert one row into the FTS table (delete + insert is simplest and correct).
pub fn fts_upsert(
    conn: &Connection,
    table: &str,
    memory_id: &str,
    text_lemmatized: Option<&str>,
) -> Result<()> {
    let fts = fts_table(table);
    conn.execute(
        &format!("DELETE FROM {fts} WHERE memory_id = ?1"),
        rusqlite::params![memory_id],
    )
    .map_err(|e| Mem0Error::vector_store(format!("fts delete {memory_id}: {e}")))?;
    if let Some(text) = text_lemmatized {
        if !text.is_empty() {
            conn.execute(
                &format!("INSERT INTO {fts}(memory_id, text_lemmatized) VALUES (?1, ?2)"),
                rusqlite::params![memory_id, text],
            )
            .map_err(|e| Mem0Error::vector_store(format!("fts insert {memory_id}: {e}")))?;
        }
    }
    Ok(())
}

/// Delete one row from the FTS table.
pub fn fts_delete(conn: &Connection, table: &str, memory_id: &str) -> Result<()> {
    conn.execute(
        &format!("DELETE FROM {} WHERE memory_id = ?1", fts_table(table)),
        rusqlite::params![memory_id],
    )
    .map_err(|e| Mem0Error::vector_store(format!("fts delete {memory_id}: {e}")))?;
    Ok(())
}

/// Escape a raw query string into a safe FTS5 MATCH phrase: wrap in double quotes
/// and double any embedded quotes. This turns arbitrary user input into a literal
/// phrase search, preventing MATCH syntax injection (e.g. `" OR "`).
pub fn escape_match_phrase(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('"');
    for ch in query.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

/// Run a keyword (BM25) search. Returns hits with positive scores (`-bm25`),
/// better matches first, exact-filtered by `filters` (scope columns inside the
/// JOIN, payload-JSON predicates outside). Returns `Some(...)`; callers that
/// want to signal "unsupported" can map to `None`.
pub fn keyword_search(
    conn: &Connection,
    table: &str,
    query: &str,
    top_k: usize,
    filters: &JsonMap,
) -> Result<Vec<SearchHit>> {
    let fts = fts_table(table);
    let compiled = compile_filters_v2(filters)?;
    // Scope-column predicates can live on the joined vec0 row (m.<col>); the
    // payload-JSON outer predicates also apply to m.payload.
    let mut conds: Vec<String> = Vec::new();
    for frag in compiled.knn_where.split(" AND ") {
        if frag.is_empty() {
            continue;
        }
        // knn fragments are written against bare column names; qualify with m.
        conds.push(format!("m.{frag}"));
    }
    for frag in compiled.outer_where.split(" AND ") {
        if frag.is_empty() {
            continue;
        }
        // outer fragments reference json_extract(payload, ...) — qualify payload
        conds.push(frag.replace("payload", "m.payload"));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        conds.join(" AND ")
    };
    let mut bind: Vec<String> = compiled.bind_values.clone();
    let match_str = escape_match_phrase(query);
    // NOTE: unnumbered `?` in the filter fragments are assigned sequentially by
    // textual position, so the filter WHERE must come before the explicitly
    // numbered MATCH/LIMIT placeholders.
    let match_clause = format!("{fts} MATCH ?{}", bind.len() + 1);
    let limit_idx = bind.len() + 2;
    let combined_where = if where_clause.is_empty() {
        format!("WHERE {match_clause}")
    } else {
        format!("WHERE {where_clause} AND {match_clause}")
    };
    let sql = format!(
        "SELECT m.memory_id, -bm25({fts}) AS score, m.payload
         FROM {fts} t JOIN {table} m ON m.memory_id = t.memory_id
         {combined_where}
         ORDER BY score DESC
         LIMIT ?{limit_idx}",
    );
    bind.push(match_str);
    bind.push((top_k as i64).to_string());

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Mem0Error::vector_store(format!("prepare keyword search: {e}")))?;
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = bind
        .iter()
        .map(|b| Box::new(b.clone()) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|e| Mem0Error::vector_store(format!("keyword search: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, score, payload) =
            row.map_err(|e| Mem0Error::vector_store(format!("keyword row: {e}")))?;
        out.push(crate::store::row_to_hit_raw(id, score as f32, payload)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escape_phrase_quotes() {
        assert_eq!(escape_match_phrase("hello"), "\"hello\"");
        assert_eq!(escape_match_phrase("a\"b"), "\"a\"\"b\"");
        assert_eq!(escape_match_phrase("\" OR \""), "\"\"\" OR \"\"\"");
    }

    #[test]
    fn fts_name() {
        assert_eq!(fts_table("mem0"), "mem0_fts");
    }

    #[test]
    fn compile_empty_filters() {
        let f = compile_filters_v2(&JsonMap::new()).unwrap();
        assert!(f.knn_where.is_empty() && f.outer_where.is_empty());
        let _ = json!(1);
    }
}
