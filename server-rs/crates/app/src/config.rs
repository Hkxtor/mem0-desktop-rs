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
            for key in ["base_url", "think_mode", "noul_threshold", "vendor"] {
                block_config.remove(key);
            }
        }
        // `think_mode` and `vendor` are leftovers from the decision vendor that was removed.
        // The current backend answers 400 for unknown fields, so a migrated config must not
        // carry them, and `vendor` itself is no longer a knob.
        if *section == "systemone" && provider == crate::categories::DECISION_PROVIDER {
            block_config.remove("think_mode");
            block_config.remove("vendor");
        }
    }
}

/// Rewrite a section's `api_keys` into the shape the pool expects, and keep the legacy
/// `api_key` pointing at the first one.
///
/// The list is the source of truth; `api_key` stays because it is what every existing config
/// file, every test fixture, and the `/configure` response already contain. Keeping it equal to
/// the first entry means a single-key config written by any previous version still works
/// untouched, and a config read by any older build still finds a key.
///
/// Order is preserved (it is the rotation order), blanks are dropped, and an exact duplicate is
/// removed: the same key listed twice is the same quota listed twice, which would only make the
/// rotation spend it twice as often.
///
/// Returns an error for a list that cannot be honoured rather than silently truncating it.
///
/// `updates` decides *which* instruction to follow, because the merge cannot tell them apart:
/// supplying `api_keys` replaces the set, supplying only `api_key` replaces it with that one key
/// (what a build that predates pools does), and supplying neither keeps what is stored. Without
/// that, a stale list left over from the merged previous config would quietly outvote a new key.
fn normalize_api_keys(config: &mut Value, updates: &Value) -> std::result::Result<(), String> {
    fn trimmed(v: Option<&Value>) -> String {
        v.and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    }
    fn dedupe(keys: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for key in keys.into_iter().filter(|k| !k.is_empty()) {
            if !out.contains(&key) {
                out.push(key);
            }
        }
        out
    }

    for section in SECTIONS {
        let instructed = updates
            .get(section)
            .and_then(|s| s.get("config"))
            .and_then(|c| c.as_object())
            .and_then(|c| {
                let listed = c
                    .get("api_keys")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(|v| trimmed(Some(v))).collect::<Vec<_>>())
                    .unwrap_or_default();
                let one = trimmed(c.get("api_key"));
                if !listed.iter().all(|k| k.is_empty()) {
                    Some(listed)
                } else if !one.is_empty() {
                    Some(vec![one])
                } else {
                    None
                }
            });

        let Some(cfg) = config
            .get_mut(section)
            .and_then(|s| s.get_mut("config"))
            .and_then(|c| c.as_object_mut())
        else {
            continue;
        };

        let keys = match instructed {
            Some(list) => dedupe(list),
            None => {
                // Nothing said about keys: re-derive the stored pair, so an alias that drifted
                // (a hand-edited file) is corrected and a key removed by a provider switch stays
                // removed instead of being resurrected by the list.
                let listed = cfg
                    .get("api_keys")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(|v| trimmed(Some(v))).collect::<Vec<_>>())
                    .unwrap_or_default();
                let one = trimmed(cfg.get("api_key"));
                let listed = dedupe(listed);
                if listed.is_empty() && one.is_empty() {
                    Vec::new()
                } else if listed.is_empty() {
                    vec![one]
                } else {
                    listed
                }
            }
        };

        if keys.len() > crate::keyring::MAX_KEYS {
            return Err(format!(
                "{section}: at most {} API keys, {} were supplied",
                crate::keyring::MAX_KEYS,
                keys.len()
            ));
        }

        match keys.len() {
            0 => {
                // Nothing usable: do not invent an empty list, and do not leave a stale one to be
                // resurrected by a later merge.
                cfg.remove("api_keys");
                cfg.remove("api_key");
            }
            _ => {
                cfg.insert(
                    "api_keys".to_string(),
                    Value::Array(keys.iter().cloned().map(Value::String).collect()),
                );
                cfg.insert("api_key".to_string(), Value::String(keys[0].clone()));
            }
        }
    }
    Ok(())
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
        // Either field counts as "the operator supplied a replacement". Checking only
        // `api_key` would let a pooled config keep the old provider's keys across a switch,
        // because the list — not the alias — is what the pool actually spends.
        let update_cfg = updates.get(section).and_then(|s| s.get("config"));
        let supplied = update_cfg
            .and_then(|c| c.get("api_key"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
            || update_cfg
                .and_then(|c| c.get("api_keys"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|v| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
                })
                .unwrap_or(false);
        if supplied {
            continue;
        }
        if let Some(cfg) = after.get_mut("config").and_then(|c| c.as_object_mut()) {
            // A key from the old provider is worse than no key: it is sent to the new
            // provider and rejected there, which reads as "the new provider is broken".
            cfg.remove("api_key");
            cfg.remove("api_keys");
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

/// Drop the removed decision vendor's endpoint.
///
/// A config written while the backend supported a second vendor still points at it. A deep
/// merge would carry that URL across every update, so classification would post memory text
/// to a service the backend no longer talks to — and the stored `api_key` would go with it.
/// The endpoint is therefore dropped unless the operator supplied one *in* this update, which
/// is them saying where to go.
///
/// `drop_stale_base_urls` cannot do this: it compares chat provider names, and the decision
/// provider has kept its name across the removal.
fn drop_removed_vendor_endpoint(previous: &Value, updates: &Value, next: &mut Value) {
    if next.pointer("/systemone/provider").and_then(|v| v.as_str())
        != Some(crate::categories::DECISION_PROVIDER)
    {
        return;
    }
    // "Explicitly Gemini" is the only shape that keeps its endpoint. Everything else on this
    // provider was written for the removed vendor: it either names it, or predates the `vendor`
    // key entirely — and either way the URL belongs to a service this build no longer calls.
    let explicit_gemini = previous
        .pointer("/systemone/config/vendor")
        .and_then(|v| v.as_str())
        .map(str::trim)
        == Some("gemini");
    if explicit_gemini {
        return;
    }
    // The dashboard pre-fills the field from the stored value and sends it back, so an
    // update carrying *the same* endpoint is an echo, not a decision. One carrying a
    // different endpoint is the operator typing a proxy, and that wins.
    let supplied = updates
        .pointer("/systemone/config/base_url")
        .or_else(|| updates.pointer("/systemone/config/systemone_base_url"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let previous_endpoint = previous
        .pointer("/systemone/config/base_url")
        .or_else(|| previous.pointer("/systemone/config/systemone_base_url"))
        .and_then(|v| v.as_str())
        .map(str::trim);
    if let Some(supplied) = supplied {
        if Some(supplied) != previous_endpoint {
            return;
        }
    }
    if let Some(cfg) = next
        .pointer_mut("/systemone/config")
        .and_then(|c| c.as_object_mut())
    {
        cfg.remove("base_url");
        cfg.remove("systemone_base_url");
    }
}

/// Convenience: apply the full update pipeline (merge → clean) the way
/// `update_config` does, returning the cleaned merged config.
pub fn apply_update(previous: &Value, updates: &mut Value) -> Value {
    apply_update_checked(previous, updates).unwrap_or_else(|_| {
        // Callers that cannot fail (tests, the overrides file) get the best-effort config; the
        // hard edges — too many keys — are surfaced by `apply_update_checked` at the HTTP edge.
        let mut next = merge_config(previous, updates);
        drop_stale_base_urls(&mut next);
        drop_removed_vendor_endpoint(previous, updates, &mut next);
        drop_replaced_credentials(previous, &mut next, updates);
        let _ = normalize_api_keys(&mut next, updates);
        next
    })
}

/// [`apply_update`], but a key list that cannot be honoured is an error instead of a silent
/// truncation. `/configure` uses this so a pasted 40-key list is a `400`, not a surprise.
pub fn apply_update_checked(
    previous: &Value,
    updates: &mut Value,
) -> std::result::Result<Value, String> {
    sync_embedding_dims(updates);
    let mut next = merge_config(previous, updates);
    drop_stale_base_urls(&mut next);
    drop_removed_vendor_endpoint(previous, updates, &mut next);
    drop_replaced_credentials(previous, &mut next, updates);
    normalize_api_keys(&mut next, updates)?;
    Ok(next)
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
                    "base_url": "https://decision.example/v1/systemone",
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

    /// The decision provider keeps its own `base_url`; what it must not keep is the removed
    /// vendor's knobs, because the API answers 400 for fields it does not know.
    #[test]
    fn the_decision_provider_keeps_its_endpoint_and_drops_the_removed_knobs() {
        let mut c = json!({
            "systemone": {
                "provider": "systemone",
                "config": {
                    "base_url": "https://generativelanguage.googleapis.com",
                    "vendor": "gemini",
                    "think_mode": "auto",
                    "noul_threshold": 0.4
                }
            }
        });
        drop_stale_base_urls(&mut c);
        let conf = &c["systemone"]["config"];
        assert_eq!(
            conf["base_url"],
            json!("https://generativelanguage.googleapis.com")
        );
        assert_eq!(conf["noul_threshold"], json!(0.4));
        assert!(conf.get("think_mode").is_none(), "{conf}");
        assert!(conf.get("vendor").is_none(), "{conf}");
    }
}

#[cfg(test)]
mod legacy_vendor_migration_tests {
    use super::apply_update;
    use serde_json::json;

    /// A config written for the removed vendor: it names it.
    fn legacy_with_vendor() -> serde_json::Value {
        json!({
            "systemone": {
                "provider": "systemone",
                "config": {
                    "vendor": "startlux",
                    "base_url": "https://api.someone-else.example/v1/systemone",
                    "api_key": "k",
                    "think_mode": "auto",
                    "noul_threshold": 0.5
                }
            }
        })
    }

    /// A config written before the vendor key existed, but on this provider: the endpoint was
    /// typed by hand, so it belongs to the removed vendor too.
    fn legacy_without_vendor() -> serde_json::Value {
        json!({
            "systemone": {
                "provider": "systemone",
                "config": {
                    "base_url": "https://api.someone-else.example/v1/systemone",
                    "model": "startlux-decision-27b-autothink",
                    "api_key": "k",
                    "noul_threshold": 0.5
                }
            }
        })
    }

    /// The endpoint belongs to the vendor that was removed. Keeping it would post memory text
    /// — and the stored key — to a service this build no longer calls, while labelling the
    /// result Gemini. A deep merge would carry it across every future update.
    #[test]
    fn a_legacy_endpoint_is_dropped_on_the_next_update() {
        for previous in [legacy_with_vendor(), legacy_without_vendor()] {
            let mut updates = json!({
                "systemone": {"provider": "systemone", "config": {"noul_threshold": 0.6}}
            });
            let next = apply_update(&previous, &mut updates);
            let conf = &next["systemone"]["config"];
            assert!(conf.get("base_url").is_none(), "{conf}");
            assert_eq!(conf["noul_threshold"], json!(0.6));
        }
    }

    /// The dashboard pre-fills the field from the stored value, so the legacy endpoint comes
    /// back in the update untouched. Treating that echo as a decision would defeat the
    /// migration for every operator who only pressed Save.
    #[test]
    fn the_stored_endpoint_echoed_back_is_also_dropped() {
        let previous = legacy_with_vendor();
        let mut updates = json!({
            "systemone": {"provider": "systemone", "config": {
                "base_url": "https://api.someone-else.example/v1/systemone"
            }}
        });
        let next = apply_update(&previous, &mut updates);
        assert!(
            next["systemone"]["config"].get("base_url").is_none(),
            "{}",
            next["systemone"]["config"]
        );
    }

    /// Supplying a *different* endpoint is the operator saying where to go.
    #[test]
    fn an_endpoint_in_the_update_survives_the_migration() {
        let mut updates = json!({
            "systemone": {"provider": "systemone", "config": {
                "base_url": "https://proxy.example/v1beta"
            }}
        });
        let next = apply_update(&legacy_with_vendor(), &mut updates);
        assert_eq!(
            next["systemone"]["config"]["base_url"],
            json!("https://proxy.example/v1beta")
        );
    }

    /// An explicitly-Gemini config was written for the current backend: its endpoint stands.
    #[test]
    fn an_explicit_gemini_endpoint_is_kept() {
        let previous = json!({
            "systemone": {
                "provider": "systemone",
                "config": {"vendor": "gemini", "base_url": "https://proxy.example/v1beta"}
            }
        });
        let mut updates = json!({
            "systemone": {"provider": "systemone", "config": {"noul_threshold": 0.7}}
        });
        let next = apply_update(&previous, &mut updates);
        assert_eq!(
            next["systemone"]["config"]["base_url"],
            json!("https://proxy.example/v1beta")
        );
    }

    /// A chat classifier never used a plain `base_url`, so the migration must not reach it.
    #[test]
    fn a_chat_classifier_is_left_alone() {
        let previous = json!({
            "systemone": {
                "provider": "openai",
                "config": {"openai_base_url": "https://token.example/v1"}
            }
        });
        let mut updates = json!({
            "systemone": {"provider": "openai", "config": {"noul_threshold": 0.9}}
        });
        let next = apply_update(&previous, &mut updates);
        assert_eq!(
            next["systemone"]["config"]["openai_base_url"],
            json!("https://token.example/v1")
        );
    }
}

#[cfg(test)]
mod api_key_pool_tests {
    use super::*;
    use serde_json::json;

    fn with_llm_keys(keys: Value) -> Value {
        json!({"llm": {"provider": "openai", "config": {"model": "gpt-4o", "api_keys": keys}}})
    }

    fn apply(previous: Value, mut updates: Value) -> Value {
        apply_update_checked(&previous, &mut updates).expect("config applies")
    }

    #[test]
    fn a_list_of_keys_is_stored_and_the_alias_follows_the_first() {
        let next = apply(json!({}), with_llm_keys(json!(["sk-1", "sk-2", "sk-3"])));
        let c = &next["llm"]["config"];
        assert_eq!(c["api_keys"], json!(["sk-1", "sk-2", "sk-3"]));
        // Every older reader still finds a key here, and it is the one rotation starts on.
        assert_eq!(c["api_key"], json!("sk-1"));
    }

    #[test]
    fn the_legacy_single_key_becomes_a_pool_of_one() {
        let next = apply(
            json!({}),
            json!({"llm": {"provider": "openai", "config": {"api_key": "sk-only"}}}),
        );
        assert_eq!(next["llm"]["config"]["api_keys"], json!(["sk-only"]));
        assert_eq!(next["llm"]["config"]["api_key"], json!("sk-only"));
    }

    #[test]
    fn saving_a_list_replaces_the_previous_one() {
        let previous = apply(json!({}), with_llm_keys(json!(["sk-1", "sk-2"])));
        let next = apply(previous, with_llm_keys(json!(["sk-9"])));
        assert_eq!(next["llm"]["config"]["api_keys"], json!(["sk-9"]));
        assert_eq!(next["llm"]["config"]["api_key"], json!("sk-9"));
    }

    #[test]
    fn a_stale_list_does_not_outvote_a_newly_supplied_single_key() {
        // A build that predates pools edits the single field. The merged config still carries
        // the old list, which must not win over the explicit instruction.
        let previous = apply(json!({}), with_llm_keys(json!(["sk-old-1", "sk-old-2"])));
        let next = apply(
            previous,
            json!({"llm": {"provider": "openai", "config": {"api_key": "sk-new"}}}),
        );
        assert_eq!(next["llm"]["config"]["api_keys"], json!(["sk-new"]));
        assert_eq!(next["llm"]["config"]["api_key"], json!("sk-new"));
    }

    #[test]
    fn an_update_that_says_nothing_about_keys_keeps_them() {
        let previous = apply(json!({}), with_llm_keys(json!(["sk-1", "sk-2"])));
        let next = apply(
            previous,
            json!({"llm": {"provider": "openai", "config": {"model": "gpt-4o-mini"}}}),
        );
        assert_eq!(next["llm"]["config"]["api_keys"], json!(["sk-1", "sk-2"]));
        assert_eq!(next["llm"]["config"]["model"], json!("gpt-4o-mini"));
    }

    #[test]
    fn blanks_are_dropped_and_repeats_collapse() {
        // The same key twice is one quota twice; keeping both would halve the rotation's reach.
        let next = apply(
            json!({}),
            with_llm_keys(json!(["sk-1", "  ", "sk-1", " sk-2 "])),
        );
        assert_eq!(next["llm"]["config"]["api_keys"], json!(["sk-1", "sk-2"]));
    }

    #[test]
    fn too_many_keys_is_an_error_rather_than_a_silent_truncation() {
        let many: Vec<String> = (0..crate::keyring::MAX_KEYS + 1)
            .map(|i| format!("sk-{i}"))
            .collect();
        let mut updates = with_llm_keys(json!(many));
        let err = apply_update_checked(&json!({}), &mut updates).unwrap_err();
        assert!(err.contains("at most"), "{err}");
        assert!(err.contains("llm"), "{err}");
    }

    #[test]
    fn exactly_the_cap_is_accepted() {
        let many: Vec<String> = (0..crate::keyring::MAX_KEYS)
            .map(|i| format!("sk-{i}"))
            .collect();
        let next = apply(json!({}), with_llm_keys(json!(many)));
        assert_eq!(
            next["llm"]["config"]["api_keys"].as_array().unwrap().len(),
            crate::keyring::MAX_KEYS
        );
    }

    #[test]
    fn switching_provider_drops_the_whole_pool_not_just_the_alias() {
        // Keeping the old provider's keys would send them to the new provider, where they are
        // rejected — which reads as "the new provider is broken".
        let previous = apply(json!({}), with_llm_keys(json!(["sk-1", "sk-2"])));
        let next = apply(
            previous,
            json!({"llm": {"provider": "anthropic", "config": {"model": "claude-haiku-4-5"}}}),
        );
        let c = &next["llm"]["config"];
        assert!(c.get("api_keys").is_none(), "{c}");
        assert!(c.get("api_key").is_none(), "{c}");
    }

    #[test]
    fn switching_provider_keeps_a_replacement_pool() {
        let previous = apply(json!({}), with_llm_keys(json!(["sk-1"])));
        let next = apply(
            previous,
            json!({"llm": {"provider": "anthropic", "config": {"api_keys": ["ant-1", "ant-2"]}}}),
        );
        assert_eq!(next["llm"]["config"]["api_keys"], json!(["ant-1", "ant-2"]));
        assert_eq!(next["llm"]["config"]["api_key"], json!("ant-1"));
    }

    #[test]
    fn the_sections_inherit_nothing_at_merge_time() {
        // Inheritance is a runtime rule (`pooled::resolve_keys`), not a config rewrite: the
        // stored config must not grow keys the operator never typed.
        let next = apply(
            json!({}),
            json!({
                "llm": {"provider": "openai", "config": {"api_keys": ["sk-1", "sk-2"]}},
                "embedder": {"provider": "openai", "config": {"model": "text-embedding-3-small"}}
            }),
        );
        assert!(next["embedder"]["config"].get("api_key").is_none());
        assert!(next["embedder"]["config"].get("api_keys").is_none());
    }
}
