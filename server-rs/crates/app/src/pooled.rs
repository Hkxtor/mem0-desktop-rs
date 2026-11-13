//! Wrappers that spend a [`Keyring`] instead of a single key.
//!
//! The fork's clients each own one key: `build_llm("openai", cfg)` reads `api_key` and is done
//! with it. Pooling therefore happens *above* them — one client per key, and a wrapper that
//! asks the ring which client to use, in this order:
//!
//! 1. `mem0::Mem0Error` carrying `429` → cool that key for its `Retry-After` (or the default)
//!    and retry the *same* call with the next key.
//! 2. `401`/`403` → retire that key for the process and retry with the next one.
//! 3. Anything else — including `5xx` and transport failures — is returned unchanged. The same
//!    upstream is broken for every key, so rotating would only multiply the wait.
//!
//! Two things this deliberately does not do: it does not sleep on a long cooldown (a desktop
//! request that hangs for a minute reads as a crash), and it does not retry a key that the
//! current request already used (that would let one unlucky request spin).
//!
//! `docs/adr/0001-api-key-pools.md` records why the pool rotates rather than failing over, and
//! why the state is in memory only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use mem0::{Embedder, GenerateOptions, Llm, Mem0Error, Message, Result};
use serde_json::Value;

use crate::keyring::{Exhausted, Keyring, Pick, PoolSnapshot, MAX_WAIT_FOR_RECOVERY};
use crate::logging;

/// The three sections that hold a key. Also the keys of the status endpoint.
pub const LLM: &str = "llm";
pub const EMBEDDER: &str = "embedder";
pub const SYSTEMONE: &str = "systemone";

/// Rings for the live runtime, by section.
///
/// A ring holds the cooldown/revocation state of the clients actually in use, so the status
/// endpoint has to read the same instance the clients spend — a ring rebuilt from config would
/// always report a healthy pool. Rebuilding a section installs a fresh ring, which is also how
/// saving new keys clears the old state.
struct Entry {
    ring: Arc<Keyring>,
    /// True when this section named no keys and spends the LLM's.
    inherited: bool,
}

fn registry() -> &'static Mutex<HashMap<&'static str, Entry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<&'static str, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_lock() -> std::sync::MutexGuard<'static, HashMap<&'static str, Entry>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Publish a section's ring, replacing (and thereby resetting) the previous one.
pub fn install(section: &'static str, ring: Arc<Keyring>, inherited: bool) -> Arc<Keyring> {
    registry_lock().insert(
        section,
        Entry {
            ring: Arc::clone(&ring),
            inherited,
        },
    );
    ring
}

/// Forget a section's ring (the section was switched off).
pub fn clear(section: &'static str) {
    registry_lock().remove(section);
}

/// Serialise the unit tests that touch the registry.
///
/// It is process-wide state, so two tests building a memory at the same time see each other's
/// sections — `a_pool_of_one_still_reports_state` would read the pool another test just
/// replaced. The integration tests take the same precaution per file.
#[cfg(test)]
pub(crate) fn test_gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forget every ring. The runtime is being torn down, so no cooldown outlives it — and a test
/// that configures the app must not inherit the previous test's pool.
pub fn reset() {
    registry_lock().clear();
}

/// The live ring for a section, if it has one.
pub fn ring(section: &'static str) -> Option<Arc<Keyring>> {
    registry_lock().get(section).map(|e| Arc::clone(&e.ring))
}

/// One section's state, as `GET /keys/status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionStatus {
    pub snapshot: PoolSnapshot,
    /// The section spends the LLM's keys because it named none of its own.
    pub inherited: bool,
}

/// Every section's state, for `GET /keys/status`. A section that was never built reports
/// `keys: 0`, which is how the UI tells "no pool here" from "a healthy pool".
pub fn snapshots() -> Vec<(&'static str, SectionStatus)> {
    [LLM, EMBEDDER, SYSTEMONE]
        .into_iter()
        .map(|section| {
            let status = registry_lock()
                .get(section)
                .map(|e| SectionStatus {
                    snapshot: e.ring.snapshot(),
                    inherited: e.inherited,
                })
                .unwrap_or(SectionStatus {
                    snapshot: PoolSnapshot::unconfigured(),
                    inherited: false,
                });
            (section, status)
        })
        .collect()
}

/// The keys a section should spend: `api_keys`, else the single legacy `api_key`.
///
/// `api_keys` wins when present so a config that carries both (the normal shape: `api_key` is
/// kept as an alias of the first entry) is not read as "one key plus a second opinion".
pub fn keys_from(cfg: &Value) -> Vec<String> {
    if let Some(arr) = cfg.get("api_keys").and_then(|v| v.as_array()) {
        let keys: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !keys.is_empty() {
            return keys;
        }
    }
    cfg.get("api_key")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .into_iter()
        .collect()
}

/// The keys a section spends, applying the inherit-from-LLM rule.
///
/// `embedder` and `systemone` may name no keys and share the LLM's provider: they then spend
/// the LLM's keys instead of none. The copies are *separate rings* — quotas are per model, so a
/// key parked by an embedding rate limit must not also be parked for chat.
pub fn resolve_keys(config: &Value, section: &'static str, provider: &str) -> (Vec<String>, bool) {
    let own = keys_from(
        config
            .get(section)
            .and_then(|s| s.get("config"))
            .unwrap_or(&Value::Null),
    );
    if !own.is_empty() || section == LLM {
        return (own, false);
    }
    let llm = config.get(LLM);
    let llm_provider = llm
        .and_then(|s| s.get("provider"))
        .and_then(|p| p.as_str())
        .unwrap_or_default();
    if llm_provider.is_empty() || llm_provider != provider {
        return (Vec::new(), false);
    }
    let inherited = keys_from(llm.and_then(|s| s.get("config")).unwrap_or(&Value::Null));
    let inherited_any = !inherited.is_empty();
    (inherited, inherited_any)
}

/// A copy of `cfg` that carries exactly one key, for building one client of the pool.
fn with_key(cfg: &Value, key: &str) -> Value {
    let mut cfg = cfg.clone();
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("api_key".to_string(), Value::String(key.to_string()));
        // The pool is the only thing that should see the list.
        obj.remove("api_keys");
    }
    cfg
}

/// The error a request gets when the pool has nothing left to spend.
///
/// Both exhaustion cases ride the `RateLimit` channel: from the caller's point of view the
/// service is temporarily unusable for a reason that is not their input, and 503 is the honest
/// status. The `code` separates "wait a moment" (`RATE_001`) from "the keys are broken"
/// (`KEYS_001`), and the message says which, because an operator reading a 503 needs to know
/// whether to retry or to open Configuration.
pub(crate) fn exhausted(section: &str, why: Exhausted, refusals: &[(usize, String)]) -> Mem0Error {
    let mut message = format!("{section}: {}", why.message());
    if matches!(why, Exhausted::AllRevoked) && !refusals.is_empty() {
        let detail: Vec<String> = refusals
            .iter()
            .map(|(idx, reason)| format!("#{} {reason}", idx + 1))
            .collect();
        message = format!("{message} — {}", detail.join("; "));
    }
    Mem0Error::RateLimit {
        code: if matches!(why, Exhausted::AllRevoked) {
            "KEYS_001".to_string()
        } else {
            "RATE_001".to_string()
        },
        message,
        suggestion: Some("Waiting for the cooldown, or fixing the key, is the only fix".into()),
    }
}

/// The error an exhausted pool reports.
///
/// `AllTried` is the defensive case (nothing new to offer because this request already spent
/// the pool); every absorbed failure removes its key, so in practice this is reached only by a
/// pool whose keys were all tried without producing a refusal.
fn pool_error(section: &'static str, ring: &Keyring, why: Exhausted) -> Mem0Error {
    exhausted(section, why, &ring.refusals())
}

/// What to do with a failed attempt, given the pool.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The pool absorbed the failure (cooled or retired the key): try the next key.
    Continue,
    /// Not a pool problem: hand the provider's own error back to the caller unchanged.
    Return,
}

/// Fold one failed attempt into the pool and say whether to try the next key.
pub(crate) fn absorb(
    section: &'static str,
    ring: &Keyring,
    idx: usize,
    err: &Mem0Error,
    tried: &[usize],
) -> Next {
    // With nothing to rotate to, parking the only key would replace the provider's own error
    // with sixty seconds of "all keys are rate limited" — worse for a caller who can retry.
    if ring.is_single() {
        let reason = match err.http_status() {
            Some(code) => format!("{code} {}", refusal_reason(err)),
            None => "error".to_string(),
        };
        ring.note_failure(idx, &reason);
        return Next::Return;
    }
    match err.http_status() {
        Some(429) => {
            let wait = err.retry_after_secs().map(Duration::from_secs);
            ring.cool(idx, wait);
            logging::warn(
                "keys",
                "api key rate limited",
                serde_json::json!({
                    "section": section,
                    "key_index": idx + 1,
                    "status": 429,
                    "cooldown_s": wait.unwrap_or(crate::keyring::DEFAULT_COOLDOWN).as_secs(),
                    "tried": tried.len() + 1,
                }),
            );
            Next::Continue
        }
        Some(code @ (401 | 403)) => {
            ring.revoke(idx, &format!("{code} {}", refusal_reason(err)));
            logging::warn(
                "keys",
                "api key rejected by the provider",
                serde_json::json!({
                    "section": section,
                    "key_index": idx + 1,
                    "status": code,
                }),
            );
            Next::Continue
        }
        _ => Next::Return,
    }
}

/// A short, key-free explanation of a refusal for the status line.
fn refusal_reason(err: &Mem0Error) -> &'static str {
    match err.http_status() {
        Some(401) => "unauthorized",
        Some(403) => "forbidden",
        _ => "rejected",
    }
}

/// Ask the ring for a key, waiting briefly when the whole pool is about to free up.
///
/// The wait is bounded by [`MAX_WAIT_FOR_RECOVERY`]: a desktop request that blocks for a full
/// cooldown looks hung, so past that point a clear 503 is better than a silent minute.
pub(crate) async fn pick_key(
    ring: &Keyring,
    tried: &[usize],
) -> std::result::Result<usize, Exhausted> {
    match ring.pick(tried) {
        Pick::Try(idx) => Ok(idx),
        Pick::Nothing(Exhausted::AllCooling { wait }) if wait <= MAX_WAIT_FOR_RECOVERY => {
            tokio::time::sleep(wait).await;
            match ring.pick(&[]) {
                Pick::Try(idx) => Ok(idx),
                Pick::Nothing(why) => Err(why),
            }
        }
        Pick::Nothing(why) => Err(why),
    }
}

/// An LLM that spends a pool.
pub struct PooledLlm {
    section: &'static str,
    ring: Arc<Keyring>,
    clients: Vec<Box<dyn Llm>>,
}

#[async_trait::async_trait]
impl Llm for PooledLlm {
    async fn generate(&self, messages: &[Message], options: &GenerateOptions) -> Result<String> {
        let mut tried: Vec<usize> = Vec::new();
        loop {
            let idx = match pick_key(&self.ring, &tried).await {
                Ok(idx) => idx,
                Err(why) => return Err(pool_error(self.section, &self.ring, why)),
            };
            match self.clients[idx].generate(messages, options).await {
                Ok(out) => {
                    self.ring.advance(idx);
                    return Ok(out);
                }
                Err(e) => {
                    let next = absorb(self.section, &self.ring, idx, &e, &tried);
                    tried.push(idx);
                    if next == Next::Return {
                        // Not a pool problem: the provider's own error, untouched.
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// An embedder that spends a pool.
pub struct PooledEmbedder {
    section: &'static str,
    ring: Arc<Keyring>,
    clients: Vec<Box<dyn Embedder>>,
    dims: usize,
}

#[async_trait::async_trait]
impl Embedder for PooledEmbedder {
    async fn embed(&self, text: &str, action: mem0::MemoryAction) -> Result<Vec<f32>> {
        let mut tried: Vec<usize> = Vec::new();
        loop {
            let idx = match pick_key(&self.ring, &tried).await {
                Ok(idx) => idx,
                Err(why) => return Err(pool_error(self.section, &self.ring, why)),
            };
            match self.clients[idx].embed(text, action).await {
                Ok(vec) => {
                    self.ring.advance(idx);
                    return Ok(vec);
                }
                Err(e) => {
                    let next = absorb(self.section, &self.ring, idx, &e, &tried);
                    tried.push(idx);
                    if next == Next::Return {
                        return Err(e);
                    }
                }
            }
        }
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

/// Build the section's LLM, pooled when it has more than one key.
///
/// A section with no key at all is not pooled: it is built exactly as before, so a provider
/// that needs no credential (mock, a local server) keeps working and a provider that does need
/// one keeps producing its own "api_key required" error.
pub fn build_llm(
    provider: &str,
    cfg: &Value,
    section: &'static str,
    keys: Vec<String>,
    inherited: bool,
) -> std::result::Result<Box<dyn Llm>, Mem0Error> {
    if keys.is_empty() {
        clear(section);
        return mem0::build_llm(provider, cfg);
    }
    let mut clients: Vec<Box<dyn Llm>> = Vec::with_capacity(keys.len());
    for key in &keys {
        clients.push(mem0::build_llm(provider, &with_key(cfg, key))?);
    }
    let ring = install(section, Arc::new(Keyring::new(keys)), inherited);
    if clients.len() == 1 {
        // The single-key shape that existed before pools, routed through the same bookkeeping
        // so the status line and the cooldowns behave identically for one key and for ten.
        return Ok(Box::new(SingleLlm {
            section,
            ring,
            client: clients.pop().expect("one client"),
        }));
    }
    Ok(Box::new(PooledLlm {
        section,
        ring,
        clients,
    }))
}

/// Build the section's embedder, pooled when it has more than one key.
pub fn build_embedder(
    provider: &str,
    cfg: &Value,
    section: &'static str,
    keys: Vec<String>,
    inherited: bool,
) -> std::result::Result<Box<dyn Embedder>, Mem0Error> {
    if keys.is_empty() {
        clear(section);
        return mem0::build_embedder(provider, cfg);
    }
    let mut clients: Vec<Box<dyn Embedder>> = Vec::with_capacity(keys.len());
    for key in &keys {
        clients.push(mem0::build_embedder(provider, &with_key(cfg, key))?);
    }
    // Every key builds the same model, so the dimensionality is the model's, not the key's.
    let dims = clients[0].dims();
    let ring = install(section, Arc::new(Keyring::new(keys)), inherited);
    Ok(Box::new(PooledEmbedder {
        section,
        ring,
        clients,
        dims,
    }))
}

/// One key, with the same cooldown and status bookkeeping as a pool.
///
/// It exists so `/keys/status` and the "all keys are rate limited" message are not lies for a
/// single-key configuration; the behaviour is otherwise what it always was: a `429` fails the
/// request with the provider's own error, because there is no other key to spend.
struct SingleLlm {
    section: &'static str,
    ring: Arc<Keyring>,
    client: Box<dyn Llm>,
}

#[async_trait::async_trait]
impl Llm for SingleLlm {
    async fn generate(&self, messages: &[Message], options: &GenerateOptions) -> Result<String> {
        match self.client.generate(messages, options).await {
            Ok(out) => {
                self.ring.advance(0);
                Ok(out)
            }
            Err(e) => {
                absorb(self.section, &self.ring, 0, &e, &[]);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_come_from_the_list_then_the_legacy_field() {
        assert_eq!(
            keys_from(&json!({"api_keys": ["a", " b ", ""], "api_key": "legacy"})),
            ["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            keys_from(&json!({"api_key": "legacy"})),
            ["legacy".to_string()]
        );
        assert!(keys_from(&json!({})).is_empty());
        assert!(keys_from(&json!({"api_keys": []})).is_empty());
        assert!(keys_from(&json!({"api_key": "   "})).is_empty());
    }

    #[test]
    fn one_client_sees_exactly_one_key_and_never_the_list() {
        let cfg = json!({"api_keys": ["a", "b"], "api_key": "a", "model": "m"});
        let one = with_key(&cfg, "b");
        assert_eq!(one["api_key"], json!("b"));
        assert!(one.get("api_keys").is_none(), "{one}");
        assert_eq!(one["model"], json!("m"));
        // The input is untouched: the pool still has both keys.
        assert_eq!(cfg["api_keys"], json!(["a", "b"]));
    }

    #[test]
    fn exhausting_the_pool_reports_a_503_shaped_error_that_never_names_a_key() {
        let e = exhausted(
            "llm",
            Exhausted::AllCooling {
                wait: Duration::from_secs(47),
            },
            &[],
        );
        assert_eq!(e.code(), Some("RATE_001"));
        assert!(e.to_string().contains("47s"), "{e}");
        assert!(!e.to_string().contains("sk-"), "{e}");

        let revoked = exhausted(
            "llm",
            Exhausted::AllRevoked,
            &[(0, "401 unauthorized".into()), (1, "403 forbidden".into())],
        );
        assert_eq!(revoked.code(), Some("KEYS_001"));
        assert!(revoked.to_string().contains("#1 401"), "{revoked}");
        assert!(revoked.to_string().contains("#2 403"), "{revoked}");
    }

    #[test]
    fn only_429_and_rejections_are_absorbed_by_the_pool() {
        let ring = Keyring::new(["a".to_string(), "b".to_string()]);
        let rate_limited = Mem0Error::llm_http(429, Some(5), "slow down");
        assert!(matches!(
            absorb("llm", &ring, 0, &rate_limited, &[]),
            Next::Continue
        ));
        let rejected = Mem0Error::llm_http(401, None, "bad key");
        assert!(matches!(
            absorb("llm", &ring, 1, &rejected, &[]),
            Next::Continue
        ));
        // A 500 is the upstream being broken: rotating would just be slower.
        let broken = Mem0Error::llm_http(500, None, "boom");
        assert!(matches!(
            absorb("llm", &ring, 0, &broken, &[]),
            Next::Return
        ));
        // So is a transport error, which carries no status at all.
        let transport = Mem0Error::llm("connection reset");
        assert!(matches!(
            absorb("llm", &ring, 0, &transport, &[]),
            Next::Return
        ));
    }

    #[test]
    fn a_pool_of_one_still_reports_state() {
        let _gate = test_gate();
        let dir = tempfile::tempdir().unwrap();
        let cfg = json!({
            "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}},
            "llm": {"provider": "mock", "config": {"api_key": "only"}},
            "embedder": {"provider": "mock", "config": {"api_key": "only"}},
            "history_db_path": ""
        });
        let _memory =
            crate::memory_bridge::build_memory(&cfg, dir.path()).expect("mock memory builds");
        let sections = snapshots();
        let (_, llm) = sections
            .iter()
            .find(|(s, _)| *s == LLM)
            .expect("llm section");
        assert_eq!(llm.snapshot.keys, 1, "{llm:?}");
        assert_eq!(llm.snapshot.cooling, 0);
        assert_eq!(llm.snapshot.revoked, 0);
        assert!(!llm.inherited);
    }
}
