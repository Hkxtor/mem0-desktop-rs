//! Bridge between the Python server's config JSON and the mem0-rs `Memory`.
//!
//! The fork's own `from_config` builds the vector store through its factory, which has
//! no sqlite-vec branch (and must not — the fork cannot depend on this repo). So this
//! module re-implements the wiring: build embedder + llm through the fork's factories,
//! build the vector store through our `vector-sqlitevec`, then hand all three to
//! `Memory::new`. Config cleanup rules from `config.rs` are applied first, exactly as
//! `server/server_state.py::update_config` does before `_build_memory`.

use std::path::Path;

use mem0::{Memory, MemoryConfig};
use mem0_core::error::{Mem0Error, Result};
use serde_json::{json, Value};

use crate::config::{apply_update, merge_config};

/// Build a `Memory` from the server-shaped config JSON.
///
/// `config_json` has the same shape the Python server persists in
/// `{data_dir}/mem0_config.json`:
/// `{ "vector_store": {"provider": ..., "config": {...}}, "llm": {...},
///    "embedder": {...}, "history_db_path": ..., ... }`.
///
/// `data_dir` overrides any path fields so the Rust app reads the same `data/`
/// layout as the Python app: `vectors.db` (sqlite-vec), `history.db`, and the
/// persisted config live there.
pub fn build_memory(config_json: &Value, data_dir: &Path) -> Result<Memory> {
    let mut cfg: MemoryConfig = serde_json::from_value(config_json.clone())
        .map_err(|e| Mem0Error::configuration(format!("parse MemoryConfig: {e}")))?;

    // Point history at the data dir unless the config already names one.
    if cfg.history_db_path.is_empty() || cfg.history_db_path == default_history_path() {
        cfg.history_db_path = data_dir.join("history.db").to_string_lossy().into_owned();
    }

    // The vector store is always sqlite-vec in the desktop product; the config's
    // provider string is honored for embedder/llm but the store is ours.
    let vs = &cfg.vector_store.config;
    let collection = vs
        .get("collection_name")
        .and_then(|v| v.as_str())
        .unwrap_or("mem0")
        .to_string();
    let dims = vs
        .get("embedding_model_dims")
        .and_then(|v| v.as_u64())
        .map(|d| d as usize);

    // Both sections may spend a pool of keys: several keys of the same provider, so one
    // exhausted quota does not fail every write. `embedder` and `systemone` may also name no
    // keys at all and inherit the LLM's — see `pooled::resolve_keys`.
    let (embedder_keys, embedder_inherited) =
        crate::pooled::resolve_keys(config_json, crate::pooled::EMBEDDER, &cfg.embedder.provider);
    let embedder = crate::pooled::build_embedder(
        &cfg.embedder.provider,
        &cfg.embedder.config,
        crate::pooled::EMBEDDER,
        embedder_keys,
        embedder_inherited,
    )
    .map_err(|e| wrap_build("embedder", &cfg.embedder.provider, e))?;
    let embedder_dims = embedder.dims();

    // Mirror the Python server: embedder dims are the single source of truth and are
    // written into the vector store config (overwriting), so a stale stored value can
    // never disagree with the embedder that will actually produce the vectors.
    let mut vs_config = cfg.vector_store.config.clone();
    if let Some(obj) = vs_config.as_object_mut() {
        obj.insert("embedding_model_dims".to_string(), json!(embedder_dims));
    }
    let dims = dims.unwrap_or(embedder_dims);

    let (llm_keys, _) =
        crate::pooled::resolve_keys(config_json, crate::pooled::LLM, &cfg.llm.provider);
    let llm = crate::pooled::build_llm(
        &cfg.llm.provider,
        &cfg.llm.config,
        crate::pooled::LLM,
        llm_keys,
        false,
    )
    .map_err(|e| wrap_build("llm", &cfg.llm.provider, e))?;

    let store = vector_sqlitevec::build(data_dir, &collection, dims)
        .map_err(|e| Mem0Error::configuration(format!("vector store: {e}")))?;

    Memory::new(cfg, embedder, llm, Box::new(store), None)
        .map_err(|e| Mem0Error::configuration(format!("Memory::new: {e}")))
}

/// Apply an update payload to a previous config, running the cleanup rules, and
/// return the cleaned merged config JSON (without building a Memory). This is the
/// pure-config half of `update_config` — the caller (M3's /configure) builds the
/// Memory from the result and rejects the update if the build fails.
pub fn compute_updated_config(previous: &Value, mut updates: Value) -> Value {
    apply_update(previous, &mut updates)
}

/// Same, but a key list that cannot be honoured is an error. `/configure` uses this so a pasted
/// list of forty keys is a `400` naming the limit, not a truncated pool the operator never chose.
pub fn compute_updated_config_checked(
    previous: &Value,
    mut updates: Value,
) -> std::result::Result<Value, String> {
    crate::config::apply_update_checked(previous, &mut updates)
}

/// Deep-merge without cleanup (exposed for tests and for the overrides file path,
/// which applies cleanup separately like the Python `_save_overrides`).
pub fn merge_only(base: &Value, updates: &Value) -> Value {
    merge_config(base, updates)
}

fn wrap_build(component: &str, provider: &str, e: Mem0Error) -> Mem0Error {
    Mem0Error::configuration(format!("{component} ({provider}): {e}"))
}

fn default_history_path() -> String {
    // Mirror the fork's mem0_dir(): $MEM0_DIR else $HOME/.mem0, then history.db.
    // The old loose match ("history.db") never matched the real default
    // ("$HOME/.mem0/history.db"), so a fresh HOME without ~/.mem0 made configure fail.
    let home = std::env::var("MEM0_DIR").unwrap_or_else(|_| {
        let h = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{h}/.mem0")
    });
    format!("{home}/history.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_memory_with_mock_providers() {
        let _gate = crate::pooled::test_gate();
        let dir = tempfile::tempdir().unwrap();
        let cfg = json!({
            "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
            "llm": {"provider": "mock", "config": {}},
            "embedder": {"provider": "mock", "config": {}},
            "history_db_path": ""
        });
        let memory = build_memory(&cfg, dir.path()).expect("mock build must succeed");
        assert!(dir.path().join("vectors.db").exists());
        assert!(dir.path().join("history.db").exists());
        drop(memory);
    }

    #[test]
    fn bad_provider_surfaces_component_and_provider_name() {
        let _gate = crate::pooled::test_gate();
        let dir = tempfile::tempdir().unwrap();
        let cfg = json!({
            "vector_store": {"provider": "sqlite-vec", "config": {"embedding_model_dims": 10}},
            "llm": {"provider": "no-such-provider", "config": {}},
            "embedder": {"provider": "mock", "config": {}},
            "history_db_path": ""
        });
        let result = build_memory(&cfg, dir.path());
        let err = match result {
            Ok(_) => panic!("bad provider must fail"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("llm"), "error must name the component: {msg}");
        assert!(
            msg.contains("no-such-provider"),
            "error must name the provider: {msg}"
        );
    }
}
