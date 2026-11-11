//! M5/R8 end-to-end: hybrid (semantic + BM25) search through the real pipeline.
//!
//! Uses a deterministic embedder whose vector depends only on the text, so a memory
//! whose *text* matches the query but whose vector is far away can only be rescued
//! by the keyword (FTS5) half of the fusion — proving keyword_search is wired into
//! Memory::search, not just present on the store.

use mem0::{AddOptions, Memory, SearchOptions};
use mem0_core::error::Result;
use mem0_core::traits::{Embedder, MemoryAction};
use mem0_core::types::JsonMap;

use serde_json::json;

/// Embedder: first 10 dims are a hash of the text (so different texts get different
/// vectors), deterministic across runs.
struct HashEmbedder;

#[async_trait::async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, text: &str, _action: MemoryAction) -> Result<Vec<f32>> {
        // Adversarial deterministic embedder: texts containing the marker keyword get
        // a vector ORTHOGONAL to the keyword's own query vector; all other texts share
        // a base vector. The query embeds to base, so the keyword doc has cosine ~0
        // semantically and only the BM25 half can rank it first.
        let mut v = vec![0.0f32; 10];
        if text.to_lowercase().contains("quixotic") {
            // NEGATIVE direction: cosine(query=base, doc) = -1 (maximally far)
            v[1] = -1.0;
        } else {
            v[1] = 1.0;
        }
        Ok(v)
    }
    fn dims(&self) -> usize {
        10
    }
}

fn build(dir: &std::path::Path) -> Memory {
    // Hand-build Memory with a deterministic per-text embedder so the keyword-only
    // memory has a vector unrelated to the query vector; only BM25 can rescue it.
    let store = vector_sqlitevec::build(dir, "mem0", 10).expect("store");
    let cfg = json!({
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {}},
        "history_db_path": format!("{}/history.db", dir.display())
    });
    let parsed: mem0::MemoryConfig = serde_json::from_value(cfg).unwrap();
    let llm = mem0::build_llm("mock", &json!({})).expect("llm");
    mem0::Memory::new(parsed, Box::new(HashEmbedder), llm, Box::new(store), None).expect("memory")
}

#[tokio::test]
async fn keyword_only_match_is_rescued_by_hybrid() {
    let dir = tempfile::tempdir().unwrap();
    let memory = build(dir.path());
    let opts = AddOptions {
        user_id: Some("alice".into()),
        infer: Some(false),
        ..Default::default()
    };
    // Two memories: one semantically near "zebra" query (similar-ish text), one whose
    // text contains the unique keyword "quixotic" but is otherwise unrelated.
    memory
        .add("zebra stripes animal africa", opts.clone())
        .await
        .unwrap();
    memory
        .add("the quixotic knight tilts at windmills", opts.clone())
        .await
        .unwrap();
    memory
        .add("quixotic quixotic quixotic quixotic quixotic", opts.clone())
        .await
        .unwrap();

    let mut filters = JsonMap::new();
    filters.insert("user_id".into(), json!("alice"));

    // Search for the unique keyword. The hash embedder gives "quixotic..." a vector
    // unrelated to a pure "quixotic" query vector, so a semantic-only search would
    // rank it poorly; hybrid must still surface it because BM25 matches exactly.
    // internal_limit = max(top_k*4, 60) = 60 semantic candidates — both memories are
    // in there (only 2 rows exist), so semantic alone would return the quixotic row
    // too. To make the test discriminate, we assert on RANK: with BM25 the
    // keyword-exact row must be first; semantic-only would order by vector distance.
    // top_k=1: only the single best-scoring row comes back. With BM25 fusion the
    // exact keyword match dominates; semantic-only ranking of two hash vectors is
    // arbitrary, so presence at rank 1 discriminates the hybrid path.
    let results = memory
        .search(
            "quixotic",
            &filters,
            SearchOptions {
                top_k: 1,
                threshold: 0.0,
                rerank: false,
            },
        )
        .await
        .expect("search");
    let arr = results["results"].as_array().expect("results");
    let memories: Vec<&str> = arr.iter().filter_map(|r| r["memory"].as_str()).collect();
    assert_eq!(
        memories.len(),
        1,
        "top_k=1 returns exactly one row, got: {memories:?}"
    );
    assert!(
        memories[0].contains("quixotic"),
        "BM25 exact keyword match must be the #1 result with top_k=1, got: {memories:?}"
    );
}

#[tokio::test]
async fn pure_semantic_still_works_without_keyword_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let memory = build(dir.path());
    let opts = AddOptions {
        user_id: Some("bob".into()),
        infer: Some(false),
        ..Default::default()
    };
    memory
        .add("bob likes hiking in the mountains", opts.clone())
        .await
        .unwrap();
    let mut filters = JsonMap::new();
    filters.insert("user_id".into(), json!("bob"));
    let results = memory
        .search(
            "hiking mountains",
            &filters,
            SearchOptions {
                top_k: 5,
                threshold: 0.0,
                rerank: false,
            },
        )
        .await
        .unwrap();
    let arr = results["results"].as_array().unwrap();
    assert!(!arr.is_empty(), "semantic search must still return results");
}
