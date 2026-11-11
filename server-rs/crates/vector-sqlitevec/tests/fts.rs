//! M5/R8: FTS5 keyword search tests for SqliteVecStore.

use mem0_core::traits::VectorStore;
use mem0_core::types::{JsonMap, VectorRecord};
use serde_json::json;
use vector_sqlitevec::{fts, SqliteVecStore};

fn make_store(dims: usize) -> (tempfile::TempDir, SqliteVecStore) {
    let dir = tempfile::tempdir().unwrap();
    let conn = vector_sqlitevec::open_db(&dir.path().join("vectors.db")).unwrap();
    let store = SqliteVecStore::new(conn, "mem0", dims).unwrap();
    (dir, store)
}

/// Open a second read connection to the store's db file for counting rows.
fn count_conn(dir: &tempfile::TempDir) -> rusqlite::Connection {
    vector_sqlitevec::open_db(&dir.path().join("vectors.db")).unwrap()
}

fn rec(id: &str, user: &str, text: &str, vec: Vec<f32>) -> VectorRecord {
    let mut payload = JsonMap::new();
    payload.insert("user_id".into(), json!(user));
    payload.insert("text".into(), json!(text));
    payload.insert("text_lemmatized".into(), json!(text.to_lowercase()));
    VectorRecord {
        id: id.into(),
        vector: vec,
        payload,
    }
}

fn vecf(dims: usize, seed: f32) -> Vec<f32> {
    (0..dims).map(|i| seed + i as f32 * 0.001).collect()
}

#[tokio::test]
async fn fts_table_created_and_backfilled() {
    let (_d, store) = make_store(8);
    let conn = count_conn(&_d);
    // FTS twin exists
    let c: i64 = conn
        .query_row("SELECT count(*) FROM mem0_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(c, 0);
    drop(conn);
}

#[tokio::test]
async fn insert_syncs_fts_and_keyword_search_hits() {
    let (_d, store) = make_store(8);
    store
        .insert(vec![
            rec("m1", "u1", "the quick brown fox", vecf(8, 0.1)),
            rec("m2", "u1", "lazy dog sleeps", vecf(8, 0.9)),
            rec("m3", "u2", "fox news tonight", vecf(8, 0.5)),
        ])
        .await
        .unwrap();
    let conn = count_conn(&_d);
    let c: i64 = conn
        .query_row("SELECT count(*) FROM mem0_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(c, 3, "every insert lands in FTS");
    drop(conn);

    let mut filters = JsonMap::new();
    filters.insert("user_id".into(), json!("u1"));
    let hits = store
        .keyword_search("fox", 10, &filters)
        .await
        .unwrap()
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, vec!["m1"], "scope filter excludes u2's fox row");
    assert!(
        hits[0].score > 0.0,
        "score must be positive (-bm25), got {}",
        hits[0].score
    );
}

#[tokio::test]
async fn keyword_score_direction_better_match_higher() {
    let (_d, store) = make_store(8);
    store
        .insert(vec![
            rec("a", "u", "apple apple apple", vecf(8, 0.1)),
            rec("b", "u", "apple banana", vecf(8, 0.2)),
        ])
        .await
        .unwrap();
    let filters = JsonMap::new();
    let hits = store
        .keyword_search("apple", 10, &filters)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert!(
        hits[0].id == "a" && hits[0].score > hits[1].score,
        "3x apple should outrank 1x apple: {hits:?}"
    );
}

#[tokio::test]
async fn delete_and_update_sync_fts() {
    let (_d, store) = make_store(8);
    store
        .insert(vec![rec("m1", "u", "hello world", vecf(8, 0.1))])
        .await
        .unwrap();
    store.delete("m1").await.unwrap();
    let conn = count_conn(&_d);
    let c: i64 = conn
        .query_row("SELECT count(*) FROM mem0_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(c, 0, "delete removes FTS row");
    drop(conn);

    store
        .insert(vec![rec("m2", "u", "hello world", vecf(8, 0.1))])
        .await
        .unwrap();
    let mut new_payload = JsonMap::new();
    new_payload.insert("user_id".into(), json!("u"));
    new_payload.insert("text_lemmatized".into(), json!("goodbye moon"));
    store.update("m2", None, Some(new_payload)).await.unwrap();
    let hits = store
        .keyword_search("hello", 10, &JsonMap::new())
        .await
        .unwrap()
        .unwrap();
    assert!(hits.is_empty(), "old text no longer matches after update");
    let hits2 = store
        .keyword_search("goodbye", 10, &JsonMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hits2.len(), 1, "new text matches after update");
}

#[tokio::test]
async fn delete_col_and_reset_clear_fts() {
    let (_d, store) = make_store(8);
    store
        .insert(vec![rec("m1", "u", "hello", vecf(8, 0.1))])
        .await
        .unwrap();
    store.delete_col().await.unwrap();
    let conn = count_conn(&_d);
    let c: i64 = conn
        .query_row("SELECT count(*) FROM mem0_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(c, 0);
    drop(conn);

    store
        .insert(vec![rec("m2", "u", "world", vecf(8, 0.1))])
        .await
        .unwrap();
    store.reset().await.unwrap();
    let conn = count_conn(&_d);
    let c: i64 = conn
        .query_row("SELECT count(*) FROM mem0_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(c, 0, "reset drops FTS twin too");
    let v: i64 = conn
        .query_row("SELECT count(*) FROM mem0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 0);
}

#[tokio::test]
async fn backfill_covers_pre_m5_database() {
    // Simulate a pre-M5 db: create vec0 table + rows WITHOUT the FTS twin, then
    // open a store and confirm backfill indexed them.
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = vector_sqlitevec::open_db(&dir.path().join("vectors.db")).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE mem0 USING vec0(
                memory_id TEXT PRIMARY KEY,
                embedding float[8] distance_metric=cosine,
                user_id TEXT, agent_id TEXT, run_id TEXT, session_id TEXT,
                payload TEXT
            )",
        )
        .unwrap();
        use zerocopy::AsBytes;
        let bytes = vecf(8, 0.1).as_bytes().to_vec();
        conn.execute(
            "INSERT INTO mem0(memory_id, embedding, user_id, agent_id, run_id, session_id, payload) VALUES ('old1', ?, 'u1', '', '', '', '{\"text_lemmatized\":\"legacy row\"}')",
            rusqlite::params![&bytes[..]],
        )
        .unwrap();
    }
    let conn = vector_sqlitevec::open_db(&dir.path().join("vectors.db")).unwrap();
    let store = SqliteVecStore::new(conn, "mem0", 8).unwrap();
    let hits = store
        .keyword_search("legacy", 10, &JsonMap::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hits.len(), 1, "backfill indexed the pre-M5 row");
    assert_eq!(hits[0].id, "old1");
}

#[tokio::test]
async fn keyword_filter_precision_500_rows_7_match() {
    // PRD hard assertion ported to keyword search: 500 rows, one user has exactly 7.
    let (_d, store) = make_store(8);
    let mut records = Vec::new();
    for i in 0..500 {
        let user = if i < 7 { "target" } else { "other" };
        records.push(rec(
            &format!("r{i}"),
            user,
            "common keyword text",
            vecf(8, i as f32 * 0.01),
        ));
    }
    store.insert(records).await.unwrap();
    let mut filters = JsonMap::new();
    filters.insert("user_id".into(), json!("target"));
    let hits = store
        .keyword_search("common", 50, &filters)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        hits.len(),
        7,
        "top_k=50 with 7 matches must return exactly 7"
    );
}

#[tokio::test]
async fn malicious_match_query_is_safe() {
    let (_d, store) = make_store(8);
    store
        .insert(vec![rec("m1", "u", "hello world", vecf(8, 0.1))])
        .await
        .unwrap();
    // FTS syntax injection attempt — must be treated as a literal phrase, not OR
    for q in ["\" OR \"", "hello OR world", "NEAR(", "*", "\"\""] {
        let r = store.keyword_search(q, 10, &JsonMap::new()).await;
        assert!(r.is_ok(), "query {q:?} must not error: {r:?}");
    }
    // escaped phrase for "hello OR world" should match nothing (no doc has that literal)
    let hits = store
        .keyword_search("\" OR \"", 10, &JsonMap::new())
        .await
        .unwrap()
        .unwrap();
    assert!(hits.is_empty());
}

#[test]
fn escape_phrase_unit() {
    assert_eq!(fts::escape_match_phrase("plain"), "\"plain\"");
    assert_eq!(fts::escape_match_phrase("a\"b"), "\"a\"\"b\"");
}
