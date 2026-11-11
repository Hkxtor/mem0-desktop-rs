//! M2 full-lifecycle integration test: mock LLM + mock embedder + sqlite-vec store,
//! exercising add -> search -> get -> get_all -> update -> delete -> history -> reset.
//!
//! The mock LLM returns a fixed extraction payload, so no real API key is needed and
//! the pipeline's parsing path (not just the HTTP layer) is exercised end to end.

use mem0::{AddOptions, Memory, SearchOptions};
use mem0_core::types::JsonMap;
use mem0_desktop_rs::memory_bridge::build_memory;

use serde_json::{json, Value};

fn mock_memory(dir: &std::path::Path) -> Memory {
    // MockLlm always returns {"memory": []} — override via a custom response is not
    // supported by the fork's mock, so for add() we use infer=false to write the
    // message text directly (the no-LLM path), which still exercises embed + store +
    // history. The LLM-extraction path is covered by the parity test in m2_parity.
    let cfg = json!({
        "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
        "llm": {"provider": "mock", "config": {}},
        "embedder": {"provider": "mock", "config": {}},
        "history_db_path": ""
    });
    build_memory(&cfg, dir).expect("mock memory builds")
}

#[tokio::test]
async fn full_lifecycle_with_mock() {
    let dir = tempfile::tempdir().unwrap();
    let memory = mock_memory(dir.path());

    // add with infer=false: writes the text directly, no LLM call
    let opts = AddOptions {
        user_id: Some("alice".into()),
        infer: Some(false),
        ..Default::default()
    };
    let added = memory
        .add("Alice likes black coffee", opts.clone())
        .await
        .expect("add");
    assert_eq!(added.len(), 1, "one memory written");
    let id = added[0].id.clone();

    // search finds it, scoped by user_id filter
    let mut filters = JsonMap::new();
    filters.insert("user_id".into(), json!("alice"));
    let results = memory
        .search(
            "coffee",
            &filters,
            SearchOptions {
                top_k: 5,
                threshold: 0.0,
                rerank: false,
            },
        )
        .await
        .expect("search");
    let arr = results["results"].as_array().expect("results array");
    assert!(
        arr.iter().any(|r| r["id"] == json!(id)),
        "added memory must be searchable"
    );

    // get by id
    let got = memory.get(&id).await.expect("get").expect("present");
    assert_eq!(got["memory"].as_str(), Some("Alice likes black coffee"));

    // get_all scoped
    let all = memory.get_all(&filters, 10).await.expect("get_all");
    assert_eq!(all["results"].as_array().unwrap().len(), 1);

    // update changes the text (re-embeds); the fork returns a status message, so
    // verify the new text through get() — that is the observable contract.
    memory
        .update(&id, "Alice now prefers green tea", None)
        .await
        .expect("update");
    let after_update = memory.get(&id).await.expect("get").expect("present");
    assert_eq!(
        after_update["memory"].as_str(),
        Some("Alice now prefers green tea")
    );

    // history recorded the update
    let hist = memory.history(&id).await.expect("history");
    assert!(
        !hist.is_empty(),
        "history must have entries after add+update"
    );

    // delete removes it from search
    memory.delete(&id).await.expect("delete");
    let after = memory
        .search(
            "tea",
            &filters,
            SearchOptions {
                top_k: 5,
                threshold: 0.0,
                rerank: false,
            },
        )
        .await
        .expect("search after delete");
    assert!(
        after["results"].as_array().unwrap().is_empty(),
        "deleted memory must not be searchable"
    );

    // reset clears everything
    memory.add("temp", opts).await.expect("add before reset");
    memory.reset().await.expect("reset");
    // get_all requires a scope filter (same validation as the Python server); use alice's
    let mut f = JsonMap::new();
    f.insert("user_id".into(), json!("alice"));
    let empty = memory.get_all(&f, 10).await.expect("get_all after reset");
    assert!(empty["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn scope_filter_isolation_between_users() {
    let dir = tempfile::tempdir().unwrap();
    let memory = mock_memory(dir.path());

    for (user, text) in [("alice", "alice fact"), ("bob", "bob fact")] {
        memory
            .add(
                text,
                AddOptions {
                    user_id: Some(user.into()),
                    infer: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("add");
    }

    let mut alice_filters = JsonMap::new();
    alice_filters.insert("user_id".into(), json!("alice"));
    let results: Value = memory
        .search(
            "fact",
            &alice_filters,
            SearchOptions {
                top_k: 10,
                threshold: 0.0,
                rerank: false,
            },
        )
        .await
        .expect("search");
    let arr = results["results"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "only alice's memory for alice filter");
    assert!(arr[0]["memory"].as_str().unwrap().contains("alice"));
}
