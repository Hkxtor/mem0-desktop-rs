//! `SqliteVecStore`: mem0-rs `VectorStore` over a sqlite-vec `vec0` virtual table.
//!
//! One row per memory: `memory_id` (primary key), `embedding` (fixed-width cosine
//! vector), `payload` (full mem0 payload as JSON text). Filters compile to
//! `json_extract(payload, '$.key')` predicates in `filters_sql`, so a filtered search
//! is an exact SQL scan — no ANN index can drop matches.

use async_trait::async_trait;
use mem0_core::error::{Mem0Error, Result};
use mem0_core::traits::VectorStore;
use mem0_core::types::{JsonMap, SearchHit, VectorRecord};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;
use zerocopy::AsBytes;

use crate::filters_sql::compile_filters_v2;

pub struct SqliteVecStore {
    conn: Mutex<Connection>,
    table: String,
    dims: usize,
}

impl SqliteVecStore {
    /// Test-only access to the shared connection (e.g. counting FTS rows).
    #[cfg(test)]
    pub(crate) fn conn_for_test(&self) -> &Mutex<Connection> {
        &self.conn
    }
}

impl SqliteVecStore {
    fn search_sync(
        &self,
        _query: &str,
        vector: &[f32],
        top_k: usize,
        filters: &JsonMap,
    ) -> Result<(Vec<SearchHit>, bool)> {
        self.check_dims(vector)?;
        let conn = self.conn.lock().map_err(poisoned)?;
        let compiled = compile_filters_v2(filters)?;

        // Exact two-stage plan:
        //  1. KNN subquery bounded by k = expand_k (>= top_k), pre-filtered by the
        //     scope-column ops vec0 supports natively inside KNN.
        //  2. Outer query applies the remaining (payload-JSON) filters to the KNN page,
        //     then LIMIT top_k. If the outer filter removed too many rows (page came up
        //     short), fall back to a full exact scan so we never under-return.
        let expand_k = (top_k * 10).max(top_k) as i64;
        let knn_where = if compiled.knn_where.is_empty() {
            String::new()
        } else {
            format!(" AND ({})", compiled.knn_where)
        };
        let outer_where = if compiled.outer_where.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", compiled.outer_where)
        };

        // Parameter order: ?1 vector blob, then knn bind values, then k, then outer values.
        let _k_idx = 2 + compiled
            .bind_values
            .iter()
            .filter(|_| compiled.knn_where.contains('?'))
            .count();
        // Simpler and robust: use named-style positional binding via a single params vec
        // in textual order. Build SQL so placeholders appear in this order:
        //   ?1 = vector, ?2.. = knn filter values, ?N = k, then outer filter values.
        let knn_vals: Vec<&String> = {
            // values pushed while compiling knn_where come first in bind_values
            // (compile_filters_v2 pushes knn-safe values before outer ones only when
            // try_knn_safe ran first; to be safe we re-split by counting '?' in knn_where)
            let n = compiled.knn_where.matches('?').count();
            compiled.bind_values.iter().take(n).collect()
        };
        let outer_vals: Vec<&String> = compiled.bind_values.iter().skip(knn_vals.len()).collect();
        let k_idx = 2 + knn_vals.len();
        let outer_start = k_idx + 1;

        let sql = format!(
            "WITH knn AS (\
           SELECT memory_id, distance FROM {table} \
           WHERE embedding MATCH ?1{knn_where} AND k = ?{k_idx} \
         )\
         SELECT knn.memory_id, knn.distance, {table}.payload \
         FROM knn JOIN {table} ON {table}.memory_id = knn.memory_id\
         {outer_where_with_idx} ORDER BY knn.distance LIMIT ?{limit_idx}",
            table = self.table,
            outer_where_with_idx = if outer_where.is_empty() {
                String::new()
            } else {
                // shift outer placeholders to start at outer_start
                let w = outer_where.clone();
                // replace each '?' in order with ?{outer_start}, ?{outer_start+1}, ...
                let mut idx = outer_start;
                let mut out = String::new();
                for ch in w.chars() {
                    if ch == '?' {
                        out.push_str(&format!("?{idx}"));
                        idx += 1;
                    } else {
                        out.push(ch);
                    }
                }
                out
            },
            limit_idx = outer_start + outer_vals.len(),
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(vector.as_bytes().to_vec()));
        for v in &knn_vals {
            params.push(Box::new((*v).clone()));
        }
        params.push(Box::new(expand_k));
        for v in &outer_vals {
            params.push(Box::new((*v).clone()));
        }
        params.push(Box::new(top_k as i64));

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| Mem0Error::vector_store(format!("prepare search: {e}")))?;
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
            .map_err(|e| Mem0Error::vector_store(format!("search: {e}")))?;
        let hits: Vec<SearchHit> = {
            let mut out = Vec::new();
            for row in rows {
                let (id, dist, payload) =
                    row.map_err(|e| Mem0Error::vector_store(format!("search row: {e}")))?;
                out.push(row_to_hit(id, dist, payload)?);
            }
            drop(stmt);
            out
        };
        drop(conn);
        let short = !compiled.outer_where.is_empty() && hits.len() < top_k;
        Ok((hits, short))
    }

    /// Wrap an open connection and ensure the collection table exists at `dims` width.
    ///
    /// An existing table whose width differs from `dims` is an error when it holds rows
    /// (those vectors cannot be reinterpreted at a new width); an empty table is simply
    /// recreated at the new width, matching the Python DuckDB provider's behaviour.
    pub fn new(conn: Connection, collection: &str, dims: usize) -> Result<Self> {
        let table = sanitize_ident(collection);
        let store = Self {
            conn: Mutex::new(conn),
            table,
            dims,
        };
        store.ensure_table()?;
        Ok(store)
    }

    fn ensure_table(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(poisoned)?;
        // mem0's primary filter keys are the scope columns (user_id / agent_id / run_id /
        // session_id). Declaring them as vec0 *metadata columns* makes `=`/`!=`/`<`/`>`
        // constraints usable inside a KNN query (vec0 only supports those operators on
        // metadata columns); everything else stays in the JSON payload and is filtered
        // post-KNN inside the CTE, which is exact.
        let expected = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING vec0(
                memory_id TEXT PRIMARY KEY,
                embedding float[{}] distance_metric=cosine,
                user_id TEXT,
                agent_id TEXT,
                run_id TEXT,
                session_id TEXT,
                payload TEXT
            )",
            self.table, self.dims
        );
        // vec0 tables cannot be altered; if the existing table has a different width and
        // holds rows, refuse rather than silently misreading old vectors.
        if let Some(width) = existing_width(&conn, &self.table)? {
            if width != self.dims {
                let rows: i64 = conn
                    .query_row(&format!("SELECT count(*) FROM {}", self.table), [], |r| {
                        r.get(0)
                    })
                    .unwrap_or(0);
                if rows > 0 {
                    return Err(Mem0Error::vector_store(format!(
                        "collection '{}' holds {rows} vectors at width {width} but config says {}; \
                         export and re-import to change dimensions",
                        self.table, self.dims
                    )));
                }
                conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", self.table))
                    .map_err(|e| Mem0Error::vector_store(format!("drop old table: {e}")))?;
            }
        }
        conn.execute_batch(&expected)
            .map_err(|e| Mem0Error::vector_store(format!("create vec0 table: {e}")))?;
        // FTS5 keyword-search twin (M5/R8). vec0 exposes no rowid, so the FTS table
        // is independent and keyed by memory_id; backfill covers pre-M5 databases.
        crate::fts::ensure_fts(&conn, &self.table)?;
        crate::fts::backfill_fts(&conn, &self.table)?;
        Ok(())
    }

    fn check_dims(&self, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dims {
            return Err(Mem0Error::vector_store(format!(
                "dimension mismatch: got {}, expected {}",
                vector.len(),
                self.dims
            )));
        }
        Ok(())
    }
}

/// Table names come from config, so quote defensively: allow only [A-Za-z0-9_] and
/// reject anything else rather than interpolating raw input into SQL.
fn sanitize_ident(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "memories".to_string()
    } else {
        clean
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Mem0Error {
    Mem0Error::vector_store("connection mutex poisoned".to_string())
}

/// Width of an existing vec0 table's embedding column, if the table exists.
fn existing_width(conn: &Connection, table: &str) -> Result<Option<usize>> {
    let sql = "SELECT sql FROM sqlite_master WHERE type IN ('table','shadow') AND name = ?1";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| Mem0Error::vector_store(format!("introspect: {e}")))?;
    let row = stmt
        .query_row(params![table], |r| r.get::<_, String>(0))
        .optional()
        .map_err(|e| Mem0Error::vector_store(format!("introspect: {e}")))?;
    Ok(row.and_then(|ddl| {
        // embedding float[N] distance_metric=cosine
        let start = ddl.find("float[")? + "float[".len();
        let end = ddl[start..].find(']')? + start;
        ddl[start..end].parse::<usize>().ok()
    }))
}

/// Build a hit from a raw similarity score (no distance->similarity conversion).
/// Used by keyword_search where  is already a positive score.
pub(crate) fn row_to_hit_raw(id: String, score: f32, payload: String) -> Result<SearchHit> {
    let payload: JsonMap = serde_json::from_str(&payload)
        .map_err(|e| Mem0Error::vector_store(format!("payload is not valid JSON: {e}")))?;
    Ok(SearchHit { id, score, payload })
}

fn row_to_hit(id: String, distance: f64, payload: String) -> Result<SearchHit> {
    let payload: JsonMap = serde_json::from_str(&payload)
        .map_err(|e| Mem0Error::vector_store(format!("payload is not valid JSON: {e}")))?;
    Ok(SearchHit {
        id,
        // cosine distance in [0, 2] -> similarity in [-1, 1]; matches pgvector.rs' 1.0 - d
        score: (1.0 - distance) as f32,
        payload,
    })
}

#[async_trait]
impl VectorStore for SqliteVecStore {
    async fn insert(&self, records: Vec<VectorRecord>) -> Result<()> {
        let mut conn = self.conn.lock().map_err(poisoned)?;
        let tx = conn
            .transaction()
            .map_err(|e| Mem0Error::vector_store(format!("begin tx: {e}")))?;
        {
            let mut stmt = tx
                .prepare(&format!(
                    "INSERT OR REPLACE INTO {} (memory_id, embedding, user_id, agent_id, run_id, session_id, payload) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    self.table
                ))
                .map_err(|e| Mem0Error::vector_store(format!("prepare insert: {e}")))?;
            for r in &records {
                self.check_dims(&r.vector)?;
                let payload = serde_json::to_string(&r.payload)
                    .map_err(|e| Mem0Error::vector_store(format!("serialize payload: {e}")))?;
                // Pull the scope columns out of the payload into their typed columns so
                // vec0 can use them in KNN WHERE; absent values bind as NULL.
                // vec0 rejects NULL for TEXT metadata columns, so absent scope values
                // bind as empty string (and "*" wildcard / != filters still behave:
                // "" IS NOT NULL is true, matching "key present" semantics closely enough
                // for the scope columns, which mem0 always sets when relevant).
                let scope = |k: &str| -> String {
                    r.payload
                        .get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                stmt.execute(params![
                    r.id,
                    r.vector.as_bytes(),
                    scope("user_id"),
                    scope("agent_id"),
                    scope("run_id"),
                    scope("session_id"),
                    payload,
                ])
                .map_err(|e| Mem0Error::vector_store(format!("insert {}: {e}", r.id)))?;
                // Keep the FTS twin in sync (M5/R8): index the lemmatized text the
                // orchestrator stores in the payload.
                let lemm = r.payload.get("text_lemmatized").and_then(|v| v.as_str());
                crate::fts::fts_upsert(&tx, &self.table, &r.id, lemm)?;
            }
        }
        tx.commit()
            .map_err(|e| Mem0Error::vector_store(format!("commit tx: {e}")))?;
        Ok(())
    }

    async fn search(
        &self,
        query: &str,
        vector: &[f32],
        top_k: usize,
        filters: &JsonMap,
    ) -> Result<Vec<SearchHit>> {
        // Do all SQLite work synchronously (the guard is !Send, so it must not live
        // across an await), then only await the fallback if needed.
        let (hits, needs_fallback) = self.search_sync(query, vector, top_k, filters)?;
        if needs_fallback {
            self.exact_scan_search(vector, top_k, filters).await
        } else {
            Ok(hits)
        }
    }

    async fn keyword_search(
        &self,
        query: &str,
        top_k: usize,
        filters: &JsonMap,
    ) -> Result<Option<Vec<SearchHit>>> {
        // FTS5 is always available (bundled with rusqlite), so we always answer
        // Some. The orchestrator fuses these BM25 scores with semantic scores.
        let conn = self.conn.lock().map_err(poisoned)?;
        Ok(Some(crate::fts::keyword_search(
            &conn,
            &self.table,
            query,
            top_k,
            filters,
        )?))
    }

    async fn get(&self, id: &str) -> Result<Option<SearchHit>> {
        let conn = self.conn.lock().map_err(poisoned)?;
        let payload: Option<String> = conn
            .query_row(
                &format!("SELECT payload FROM {} WHERE memory_id = ?1", self.table),
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Mem0Error::vector_store(format!("get {id}: {e}")))?;
        Ok(payload
            .map(|p| row_to_hit(id.to_string(), 0.0, p))
            .transpose()?)
    }

    async fn update(
        &self,
        id: &str,
        vector: Option<Vec<f32>>,
        payload: Option<JsonMap>,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(poisoned)?;
        if let Some(v) = &vector {
            self.check_dims(v)?;
            conn.execute(
                &format!(
                    "UPDATE {} SET embedding = ?1 WHERE memory_id = ?2",
                    self.table
                ),
                params![v.as_bytes(), id],
            )
            .map_err(|e| Mem0Error::vector_store(format!("update vector {id}: {e}")))?;
        }
        if let Some(p) = &payload {
            let text = serde_json::to_string(p)
                .map_err(|e| Mem0Error::vector_store(format!("serialize payload: {e}")))?;
            conn.execute(
                &format!(
                    "UPDATE {} SET payload = ?1 WHERE memory_id = ?2",
                    self.table
                ),
                params![text, id],
            )
            .map_err(|e| Mem0Error::vector_store(format!("update payload {id}: {e}")))?;
            // Re-sync the FTS twin with the new payload's lemmatized text.
            let lemm = p.get("text_lemmatized").and_then(|v| v.as_str());
            crate::fts::fts_upsert(&conn, &self.table, id, lemm)?;
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(poisoned)?;
        conn.execute(
            &format!("DELETE FROM {} WHERE memory_id = ?1", self.table),
            params![id],
        )
        .map_err(|e| Mem0Error::vector_store(format!("delete {id}: {e}")))?;
        crate::fts::fts_delete(&conn, &self.table, id)?;
        Ok(())
    }

    async fn list(&self, filters: &JsonMap, limit: Option<usize>) -> Result<Vec<SearchHit>> {
        let conn = self.conn.lock().map_err(poisoned)?;
        let compiled = compile_filters_v2(filters)?;
        let where_sql = {
            let mut w = compiled.knn_where.clone();
            if !compiled.outer_where.is_empty() {
                if !w.is_empty() {
                    w.push_str(" AND ");
                }
                w.push_str(&compiled.outer_where);
            }
            w
        };
        let bind_values = compiled.bind_values;
        let where_clause = if where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {where_sql}")
        };
        let limit = limit.unwrap_or(10_000) as i64;
        let sql = format!(
            "SELECT memory_id, payload FROM {} {where_clause} LIMIT ?",
            self.table
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| Mem0Error::vector_store(format!("prepare list: {e}")))?;
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        for v in &bind_values {
            params.push(Box::new(v.clone()));
        }
        params.push(Box::new(limit));
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| Mem0Error::vector_store(format!("list: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, payload) =
                row.map_err(|e| Mem0Error::vector_store(format!("list row: {e}")))?;
            // No distance was computed, so there is no similarity to report. Going through
            // `row_to_hit(id, 0.0, …)` turned that into score 1.0 and leaked a fabricated
            // `score` field into listings Python never emits.
            out.push(row_to_hit_raw(id, 0.0, payload)?);
        }
        Ok(out)
    }

    async fn delete_col(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(poisoned)?;
        conn.execute(&format!("DELETE FROM {}", self.table), [])
            .map_err(|e| Mem0Error::vector_store(format!("delete_col: {e}")))?;
        conn.execute(
            &format!("DELETE FROM {}", crate::fts::fts_table(&self.table)),
            [],
        )
        .map_err(|e| Mem0Error::vector_store(format!("delete_col fts: {e}")))?;
        Ok(())
    }

    async fn reset(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(poisoned)?;
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", self.table))
            .map_err(|e| Mem0Error::vector_store(format!("drop: {e}")))?;
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS {}",
            crate::fts::fts_table(&self.table)
        ))
        .map_err(|e| Mem0Error::vector_store(format!("drop fts: {e}")))?;
        drop(conn);
        self.ensure_table()
    }
}

impl SqliteVecStore {
    /// Full exact scan: no KNN, just filter + ORDER BY distance over all rows.
    /// Used as the fallback when the KNN page came up short after outer filtering,
    /// guaranteeing search is never approximate.
    async fn exact_scan_search(
        &self,
        vector: &[f32],
        top_k: usize,
        filters: &JsonMap,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.conn.lock().map_err(poisoned)?;
        let compiled = compile_filters_v2(filters)?;
        let all_where = {
            let mut w = compiled.knn_where.clone();
            if !compiled.outer_where.is_empty() {
                if !w.is_empty() {
                    w.push_str(" AND ");
                }
                w.push_str(&compiled.outer_where);
            }
            w
        };
        let where_clause = if all_where.is_empty() {
            String::new()
        } else {
            format!("WHERE {all_where}")
        };
        // Plain scan: compute cosine distance with vec_distance_cosine. Parameter indices:
        // ?1 = query vector, then the filter placeholders (knn+outer merged), then LIMIT.
        let n_filter = compiled.bind_values.len();
        let limit_idx = 2 + n_filter;
        let sql = format!(
        "SELECT memory_id, vec_distance_cosine(embedding, ?1) AS distance, payload FROM {table} {where_clause} ORDER BY distance LIMIT ?{limit_idx}",
        table = self.table,
    );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(vector.as_bytes().to_vec()));
        for v in &compiled.bind_values {
            params.push(Box::new(v.clone()));
        }
        params.push(Box::new(top_k as i64));
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| Mem0Error::vector_store(format!("prepare exact scan: {e}")))?;
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
            .map_err(|e| Mem0Error::vector_store(format!("exact scan: {e}")))?;
        let mut hits = Vec::new();
        for row in rows {
            let (id, dist, payload) =
                row.map_err(|e| Mem0Error::vector_store(format!("exact scan row: {e}")))?;
            hits.push(row_to_hit(id, dist, payload)?);
        }
        Ok(hits)
    }
}
