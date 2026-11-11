//! M5/R9 perf baseline: inserts 10k/100k rows and measures search + keyword p50/p95.
//! Prints ONE line of JSON to stdout for CI to parse, e.g.
//!   {"insert_10k_ms":..,"insert_100k_ms":..,"search_10k_p95_ms":..,
//!    "search_100k_p95_ms":..,"search_100k_filtered_p95_ms":..,"keyword_100k_p95_ms":..}
//!
//! Run: cargo run --release -p vector-sqlitevec --bin perf

use mem0_core::traits::VectorStore;
use mem0_core::types::{JsonMap, VectorRecord};
use serde_json::json;
use std::time::{Duration, Instant};

fn vec_for(id: u64, dims: usize) -> Vec<f32> {
    (0..dims)
        .map(|i| {
            let x = (id.wrapping_mul(2654435761) ^ (i as u64).wrapping_mul(40503)) % 2000;
            (x as f32 - 1000.0) / 1000.0
        })
        .collect()
}

fn rec(id: u64, dims: usize) -> VectorRecord {
    let mut payload = JsonMap::new();
    payload.insert(
        "user_id".into(),
        json!(if id % 10 == 0 { "target" } else { "u" }),
    );
    payload.insert("text".into(), json!(format!("memory number {id}")));
    payload.insert(
        "text_lemmatized".into(),
        json!(format!("memory number {id}")),
    );
    VectorRecord {
        id: format!("m{id}"),
        vector: vec_for(id, dims),
        payload,
    }
}

fn percentile(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort();
    let idx = ((v.len() as f64 * p).ceil() as usize)
        .saturating_sub(1)
        .min(v.len() - 1);
    v[idx]
}

async fn search_times(
    store: &vector_sqlitevec::SqliteVecStore,
    q: &[f32],
    filters: &JsonMap,
    top_k: usize,
    iters: usize,
) -> Vec<Duration> {
    // warmup
    let _ = store.search("q", q, top_k, filters).await.expect("warmup");
    let mut times = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let hits = store.search("q", q, top_k, filters).await.expect("search");
        times.push(t.elapsed());
        assert_eq!(hits.len(), top_k);
    }
    times
}

async fn keyword_times(
    store: &vector_sqlitevec::SqliteVecStore,
    query: &str,
    filters: &JsonMap,
    iters: usize,
) -> Vec<Duration> {
    let mut times = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let hits = store.keyword_search(query, 50, filters).await.unwrap();
        times.push(t.elapsed());
        assert!(hits.is_some());
    }
    times
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dims: usize = std::env::var("PERF_DIMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1536);
    let pid = std::process::id();
    let mut report = json!({});

    // --- 10k store ---
    let dir10k = std::env::temp_dir().join(format!("mem0-perf10k-{pid}"));
    let _ = std::fs::remove_dir_all(&dir10k);
    let conn = vector_sqlitevec::open_db(&dir10k.join("vectors.db")).unwrap();
    let store10k = vector_sqlitevec::SqliteVecStore::new(conn, "mem0", dims).unwrap();
    let t = Instant::now();
    let records: Vec<VectorRecord> = (0..10_000u64).map(|i| rec(i, dims)).collect();
    store10k.insert(records).await.expect("insert 10k");
    report["insert_10k_ms"] = json!(t.elapsed().as_millis() as u64);

    let q = vec_for(999_999, dims);
    let all = JsonMap::new();
    let times = search_times(&store10k, &q, &all, 50, 20).await;
    report["search_10k_p50_ms"] = json!(percentile(times.clone(), 0.50).as_millis() as u64);
    report["search_10k_p95_ms"] = json!(percentile(times, 0.95).as_millis() as u64);

    // --- 100k store ---
    let dir100k = std::env::temp_dir().join(format!("mem0-perf100k-{pid}"));
    let _ = std::fs::remove_dir_all(&dir100k);
    let conn = vector_sqlitevec::open_db(&dir100k.join("vectors.db")).unwrap();
    let store100k = vector_sqlitevec::SqliteVecStore::new(conn, "mem0", dims).unwrap();
    let t = Instant::now();
    let n100k: u64 = std::env::var("PERF_ROWS_100K")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    for chunk in (10_000..n100k).collect::<Vec<_>>().chunks(10_000) {
        let records: Vec<VectorRecord> = chunk.iter().map(|&i| rec(i, dims)).collect();
        store100k.insert(records).await.expect("insert chunk");
    }
    report["insert_100k_ms"] = json!(t.elapsed().as_millis() as u64);

    let times = search_times(&store100k, &q, &all, 50, 20).await;
    report["search_100k_p95_ms"] = json!(percentile(times, 0.95).as_millis() as u64);

    let mut f_target = JsonMap::new();
    f_target.insert("user_id".into(), json!("target"));
    let times = search_times(&store100k, &q, &f_target, 50, 20).await;
    report["search_100k_filtered_p95_ms"] = json!(percentile(times, 0.95).as_millis() as u64);

    let times = keyword_times(&store100k, "memory number", &all, 20).await;
    report["keyword_100k_p95_ms"] = json!(percentile(times, 0.95).as_millis() as u64);

    println!("{}", serde_json::to_string(&report).unwrap());
    let _ = std::fs::remove_dir_all(&dir10k);
    let _ = std::fs::remove_dir_all(&dir100k);
}
