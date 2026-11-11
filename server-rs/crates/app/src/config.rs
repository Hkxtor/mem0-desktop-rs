//! Config cleanup rules ported from the Python server (`server/server_state.py`,
//! `server/main.py`).
//!
//! These four rules exist because each one fixed a real production bug in the Python
//! product — a stale `anthropic_base_url` left in a merged config made provider switches
//! fail with `TypeError: unexpected keyword argument` *after a restart*, and a stale
//! `api_key` survived a provider switch and produced a 401 that named neither the
//! provider nor the switch. The Rust rewrite must not reintroduce them, so the rules
//! are implemented here with unit tests mirroring `tests/test_server_*.py`.

use serde_json::{json, Map, Value};

/// Providers the desktop product can select, with a base URL key each
/// (`server/main.py`: `BUNDLED_LLM_PROVIDERS`, `PROVIDERS_WITH_BASE_URL`).
pub const PROVIDERS_WITH_BASE_URL: &[&str] = &["openai", "anthropic", "gemini"];

/// Sections that carry a provider + config block.
const SECTIONS: &[&str] = &["llm", "embedder", "systemone"];

/// Drop `{other_provider}_base_url` keys from each section when the section's
/// provider is not that provider. Port of `_drop_stale_base_urls`
/// (`server/server_state.py:162`).
///
/// A provider's base URL belongs to that provider: switching provider leaves the
/// previous one's key in the merged config, and the next provider's config struct
/// rejects keywords it does not define.
pub fn drop_stale_base_urls(config: &mut Value) {
    for section in SECTIONS {
        let Some(block) = config.get_mut(section).and_then(|b| b.as_object_mut()) else {
            continue;
        };
        let provider = block
            .get("provider")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let Some(provider) = provider else { continue };
        let Some(block_config) = block.get_mut("config").and_then(|c| c.as_object_mut()) else {
            continue;
        };
        let stale: Vec<String> = PROVIDERS_WITH_BASE_URL
            .iter()
            .filter(|name| **name != provider)
            .map(|name| format!("{name}_base_url"))
            .filter(|key| block_config.contains_key(key))
            .collect();
        for key in stale {
            block_config.remove(&key);
        }
        // The SystemOne decision backend is not a chat provider: it keeps its endpoint
        // under a plain `base_url` and adds `think_mode` / `noul_threshold`. Leaving
        // them behind when the section switches to a chat provider makes the UI's base
        // URL fall back to the decision endpoint (`.../v1/systemone`), which is precisely
        // the 404 this cleanup exists to prevent.
        if *section == "systemone" && provider != crate::categories::DECISION_PROVIDER {
            for key in ["base_url", "think_mode", "noul_threshold"] {
                block_config.remove(key);
            }
        }
    }
}

/// Drop a section's `api_key` when the provider changed and the update did not
/// supply a replacement key. Port of `_drop_replaced_credentials`
/// (`server/server_state.py`).
///
/// `previous` = config before the update, `updates` = the raw update payload,
/// `next_config` = merged config after the update (mutated in place).
pub fn drop_replaced_credentials(previous: &Value, next_config: &mut Value, updates: &Value) {
    for section in SECTIONS {
        let (Some(before), Some(after)) = (
            previous.get(section).and_then(|b| b.as_object()),
            next_config.get_mut(section).and_then(|b| b.as_object_mut()),
        ) else {
            continue;
        };
        let (Some(old_provider), Some(new_provider)) = (
            before.get("provider").and_then(|v| v.as_str()),
            after.get("provider").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if old_provider.is_empty() || new_provider.is_empty() || old_provider == new_provider {
            continue;
        }
        let supplied = updates
            .get(section)
            .and_then(|s| s.get("config"))
            .and_then(|c| c.get("api_key"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if supplied {
            continue;
        }
        if let Some(cfg) = after.get_mut("config").and_then(|c| c.as_object_mut()) {
            cfg.remove("api_key");
        }
    }
}

/// Make the embedder's `embedding_dims` the single source of truth for the vector
/// store's `embedding_model_dims`. Port of `_sync_embedding_dims`
/// (`server/main.py:328`): only acts when the update actually mentions
/// `embedder.config.embedding_dims`, so an LLM-only change never resets the width.
pub fn sync_embedding_dims(updates: &mut Value) {
    let Some(embedder) = updates.get("embedder").and_then(|e| e.as_object()) else {
        return;
    };
    let Some(dims) = embedder
        .get("config")
        .and_then(|c| c.get("embedding_dims"))
        .and_then(|v| v.as_u64())
    else {
        return;
    };
    let vector_store = updates
        .as_object_mut()
        .expect("updates is an object")
        .entry("vector_store".to_string())
        .or_insert_with(|| json!({}));
    let store_config = vector_store
        .as_object_mut()
        .expect("vector_store is an object")
        .entry("config".to_string())
        .or_insert_with(|| json!({}));
    store_config
        .as_object_mut()
        .expect("config is an object")
        .insert("embedding_model_dims".to_string(), json!(dims));
}

/// Deep-merge `updates` into `base` (objects merge recursively, everything else
/// is replaced). Port of `_merge_config` in `server/server_state.py`.
pub fn merge_config(base: &Value, updates: &Value) -> Value {
    match (base, updates) {
        (Value::Object(b), Value::Object(u)) => {
            let mut out = b.clone();
            for (k, v) in u {
                let merged = match out.get(k) {
                    Some(existing) => merge_config(existing, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => updates.clone(),
    }
}

/// Convenience: apply the full update pipeline (merge → clean) the way
/// `update_config` does, returning the cleaned merged config.
pub fn apply_update(previous: &Value, updates: &mut Value) -> Value {
    sync_embedding_dims(updates);
    let mut next = merge_config(previous, updates);
    drop_stale_base_urls(&mut next);
    drop_replaced_credentials(previous, &mut next, updates);
    next
}

/// Extract a mutable reference to a section's config object, if present.
pub fn section_config_mut<'a>(
    config: &'a mut Value,
    section: &str,
) -> Option<&'a mut Map<String, Value>> {
    config.get_mut(section)?.get_mut("config")?.as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: &str, config: Value) -> Value {
        json!({"llm": {"provider": provider, "config": config}})
    }

    #[test]
    fn stale_base_url_of_other_provider_is_dropped() {
        // the exact bug: switching to openai left anthropic_base_url in the merged
        // config and OpenAIConfig rejected the unknown keyword
        let mut c = cfg(
            "openai",
            json!({"api_key": "k", "anthropic_base_url": "https://x"}),
        );
        drop_stale_base_urls(&mut c);
        let cc = c["llm"]["config"].as_object().unwrap();
        assert!(!cc.contains_key("anthropic_base_url"));
        assert_eq!(cc.get("api_key").and_then(|v| v.as_str()), Some("k"));
    }

    #[test]
    fn own_base_url_is_kept() {
        let mut c = cfg("anthropic", json!({"anthropic_base_url": "https://x"}));
        drop_stale_base_urls(&mut c);
        assert!(c["llm"]["config"]
            .as_object()
            .unwrap()
            .contains_key("anthropic_base_url"));
    }

    #[test]
    fn provider_switch_without_new_key_drops_old_key() {
        let prev = cfg("openai", json!({"api_key": "sk-openai"}));
        let mut updates = json!({"llm": {"provider": "anthropic"}});
        let next = apply_update(&prev, &mut updates);
        assert_eq!(next["llm"]["provider"], json!("anthropic"));
        assert!(next["llm"]["config"].get("api_key").is_none());
    }

    #[test]
    fn provider_switch_with_new_key_keeps_it() {
        let prev = cfg("openai", json!({"api_key": "sk-openai"}));
        let mut updates =
            json!({"llm": {"provider": "anthropic", "config": {"api_key": "sk-ant"}}});
        let next = apply_update(&prev, &mut updates);
        assert_eq!(next["llm"]["config"]["api_key"], json!("sk-ant"));
    }

    #[test]
    fn model_only_change_keeps_key() {
        let prev = cfg("openai", json!({"api_key": "sk-openai", "model": "gpt-4o"}));
        let mut updates = json!({"llm": {"config": {"model": "gpt-5"}}});
        let next = apply_update(&prev, &mut updates);
        assert_eq!(next["llm"]["config"]["api_key"], json!("sk-openai"));
        assert_eq!(next["llm"]["config"]["model"], json!("gpt-5"));
    }

    #[test]
    fn embedder_dims_override_store_dims() {
        let mut updates =
            json!({"embedder": {"provider": "gemini", "config": {"embedding_dims": 768}}});
        sync_embedding_dims(&mut updates);
        assert_eq!(
            updates["vector_store"]["config"]["embedding_model_dims"],
            json!(768)
        );
    }

    #[test]
    fn llm_only_update_does_not_touch_dims() {
        let mut updates = json!({"llm": {"config": {"model": "gpt-5"}}});
        sync_embedding_dims(&mut updates);
        assert!(updates.get("vector_store").is_none());
    }

    #[test]
    fn merge_is_deep_for_objects() {
        let base =
            json!({"llm": {"provider": "openai", "config": {"api_key": "k", "model": "gpt-4o"}}});
        let updates = json!({"llm": {"config": {"model": "gpt-5"}}});
        let merged = merge_config(&base, &updates);
        assert_eq!(merged["llm"]["config"]["api_key"], json!("k"));
        assert_eq!(merged["llm"]["config"]["model"], json!("gpt-5"));
    }
}

#[cfg(test)]
mod decision_key_tests {
    use super::drop_stale_base_urls;
    use serde_json::json;

    #[test]
    fn switching_systemone_to_a_chat_provider_drops_decision_keys() {
        // The reported shape: decision provider configured, then switched to openai.
        // The leftover plain `base_url` made the UI show .../v1/systemone for a chat
        // client, which appends /chat/completions and 404s.
        let mut c = json!({
            "systemone": {
                "provider": "openai",
                "config": {
                    "model": "sensenova-6.8-flash",
                    "openai_base_url": "https://token.example/v1",
                    "base_url": "https://api.bamboonode.dev/v1/systemone",
                    "think_mode": "auto",
                    "noul_threshold": 0.5
                }
            }
        });
        drop_stale_base_urls(&mut c);
        let conf = &c["systemone"]["config"];
        assert_eq!(conf["openai_base_url"], json!("https://token.example/v1"));
        assert!(conf.get("base_url").is_none(), "{conf}");
        assert!(conf.get("think_mode").is_none(), "{conf}");
        assert!(conf.get("noul_threshold").is_none(), "{conf}");
    }

    #[test]
    fn the_decision_provider_keeps_its_own_keys() {
        let mut c = json!({
            "systemone": {
                "provider": "systemone",
                "config": {
                    "base_url": "https://api.bamboonode.dev/v1/systemone",
                    "think_mode": "auto",
                    "noul_threshold": 0.4
                }
            }
        });
        drop_stale_base_urls(&mut c);
        let conf = &c["systemone"]["config"];
        assert_eq!(
            conf["base_url"],
            json!("https://api.bamboonode.dev/v1/systemone")
        );
        assert_eq!(conf["think_mode"], json!("auto"));
        assert_eq!(conf["noul_threshold"], json!(0.4));
    }
}
