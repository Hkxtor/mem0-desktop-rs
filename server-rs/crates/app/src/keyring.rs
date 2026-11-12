//! API key pools: which key to spend, and when to stop spending one.
//!
//! One key is one quota. A pool spreads requests across several keys of the same section and
//! takes a key out of circulation when the upstream says to: `429` parks it for the upstream's
//! `Retry-After` (or [`DEFAULT_COOLDOWN`]), `401`/`403` retires it for the life of the process.
//! See `docs/adr/0001-api-key-pools.md` for why the pool rotates at all and why waiting inside
//! a request is bounded.
//!
//! This module is the *bookkeeping* only — no HTTP, no provider knowledge. [`crate::pooled`]
//! drives it: ask [`Keyring::pick`] for a key, attempt the call, report the outcome with
//! [`Keyring::cool`] / [`Keyring::revoke`] / [`Keyring::advance`], repeat.
//!
//! Two invariants matter:
//!
//! 1. **A key is tried at most once per request.** Without that, one request against three
//!    rate-limited keys would loop forever.
//! 2. **A pool of one behaves exactly like no pool.** Every state here only removes a key that
//!    was already refused, so a single-key configuration fails the way it always did, with the
//!    provider's own error.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How many keys one section may hold. A dozen is generous; the cap exists so a pasted file
/// cannot turn a rotation into a hundred upstream calls before failing.
pub const MAX_KEYS: usize = 20;

/// How long a key is parked after a `429` when the upstream did not say.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

/// The longest wait a single request will do for a cooling key. Past this, failing with a
/// clear message beats holding an interactive request open.
pub const MAX_WAIT_FOR_RECOVERY: Duration = Duration::from_secs(3);

/// What a caller should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    /// Try this key (index into [`Keyring::keys`]).
    Try(usize),
    /// No key can be tried for this request.
    Nothing(Exhausted),
}

/// Why a request has nowhere to go.
#[derive(Debug, PartialEq, Eq)]
pub enum Exhausted {
    /// The section has no keys at all. Not an error the pool can act on: something else
    /// (a provider that needs no key, or a configuration that never had one) decides what to do.
    Empty,
    /// Every key was refused as a credential. Waiting will never fix this.
    AllRevoked,
    /// Every key is rate limited. `wait` is until the earliest one frees up.
    AllCooling { wait: Duration },
    /// Every key was already attempted by this request (and none reported a rate limit).
    /// The caller returns the last upstream error rather than inventing one.
    AllTried,
}

impl Exhausted {
    /// The operator-facing sentence. Names the situation, never a key.
    pub fn message(&self) -> String {
        match self {
            Exhausted::Empty => "no API key configured".to_string(),
            Exhausted::AllRevoked => {
                "every API key was rejected by the provider (401/403); fix the keys to continue"
                    .to_string()
            }
            Exhausted::AllCooling { wait } => format!(
                "all API keys are rate limited; the earliest recovers in {}s",
                wait.as_secs().max(1)
            ),
            Exhausted::AllTried => "every API key was already tried for this request".to_string(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct KeyState {
    /// When this key may be used again, if it was rate limited.
    cooling_until: Option<Instant>,
    /// The provider rejected the credential. Permanent for this process.
    revoked: bool,
    /// The most recent refusal, for the status endpoint and the log.
    last_error: Option<String>,
}

/// A pool of interchangeable keys for one model section.
#[derive(Debug)]
pub struct Keyring {
    keys: Vec<String>,
    state: Mutex<PoolState>,
}

#[derive(Debug, Default)]
struct PoolState {
    keys: Vec<KeyState>,
    /// Where the next request starts, so traffic rotates instead of hammering key one.
    cursor: usize,
}

impl Keyring {
    /// Build a ring over `keys`. Empty keys are dropped; a duplicate is the same quota, so it
    /// is dropped too rather than doubling that key's share of the rotation.
    pub fn new(keys: impl IntoIterator<Item = String>) -> Self {
        let mut unique: Vec<String> = Vec::new();
        for key in keys {
            let key = key.trim().to_string();
            if key.is_empty() || unique.contains(&key) {
                continue;
            }
            unique.push(key);
            if unique.len() == MAX_KEYS {
                break;
            }
        }
        let state = PoolState {
            keys: vec![KeyState::default(); unique.len()],
            cursor: 0,
        };
        Self {
            keys: unique,
            state: Mutex::new(state),
        }
    }

    /// A ring holding a single key. The shape every configuration had before pools existed.
    pub fn single(key: impl Into<String>) -> Self {
        Self::new([key.into()])
    }

    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// One key means there is nothing to rotate to, so a refusal must surface as the
    /// provider's own error rather than as pool bookkeeping.
    pub fn is_single(&self) -> bool {
        self.keys.len() <= 1
    }

    /// Which keys were retired and why, for the "every key was rejected" message.
    /// Index plus reason — never the key itself.
    pub fn refusals(&self) -> Vec<(usize, String)> {
        let st = self.lock();
        st.keys
            .iter()
            .enumerate()
            .filter(|(_, s)| s.revoked)
            .filter_map(|(i, s)| s.last_error.clone().map(|e| (i, e)))
            .collect()
    }

    /// Record why a key was refused without taking it out of circulation.
    ///
    /// Used for a single-key pool: the pool has no alternative to offer, so parking the only
    /// key would turn the provider's `429` into sixty seconds of "all keys are rate limited"
    /// for a caller who could simply retry.
    pub fn note_failure(&self, idx: usize, reason: &str) {
        let mut st = self.lock();
        if let Some(state) = st.keys.get_mut(idx) {
            state.last_error = Some(reason.to_string());
        }
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        // A panic while holding this lock would leave the pool in a half-updated state, but
        // the updates are single-field writes with no invariant between them, so the worst
        // case is a lost cooldown. Poisoning must not take the whole server down.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next key to try, skipping `tried` (the keys this request already spent).
    pub fn pick(&self, tried: &[usize]) -> Pick {
        if self.keys.is_empty() {
            return Pick::Nothing(Exhausted::Empty);
        }
        let st = self.lock();
        let now = Instant::now();
        let n = self.keys.len();

        // Walk the ring starting at the cursor so successive requests land on different keys.
        let mut cooling: Option<Duration> = None;
        let mut any_live = false;
        for step in 0..n {
            let idx = (st.cursor + step) % n;
            let state = &st.keys[idx];
            if state.revoked {
                continue;
            }
            any_live = true;
            match state.cooling_until {
                Some(until) if until > now => {
                    // Cooling counts even when this request already tried the key: the reply
                    // it got is the reason it is cooling.
                    let wait = until - now;
                    cooling = Some(cooling.map_or(wait, |w: Duration| w.min(wait)));
                }
                _ if tried.contains(&idx) => {}
                _ => return Pick::Try(idx),
            }
        }

        if let Some(wait) = cooling {
            return Pick::Nothing(Exhausted::AllCooling { wait });
        }
        if !any_live {
            return Pick::Nothing(Exhausted::AllRevoked);
        }
        Pick::Nothing(Exhausted::AllTried)
    }

    /// The call with this key succeeded: the next request starts after it.
    pub fn advance(&self, idx: usize) {
        let mut st = self.lock();
        if self.keys.is_empty() {
            return;
        }
        st.cursor = (idx + 1) % self.keys.len();
        if let Some(state) = st.keys.get_mut(idx) {
            state.last_error = None;
        }
    }

    /// The upstream rate limited this key. `wait` is its `Retry-After`, if it sent one.
    pub fn cool(&self, idx: usize, wait: Option<Duration>) {
        let mut st = self.lock();
        if let Some(state) = st.keys.get_mut(idx) {
            state.cooling_until = Some(Instant::now() + wait.unwrap_or(DEFAULT_COOLDOWN));
            state.last_error = Some("429 rate limited".to_string());
        }
        if !self.keys.is_empty() {
            st.cursor = (idx + 1) % self.keys.len();
        }
    }

    /// The provider rejected this key's credential (401/403). It stays out until the process
    /// restarts or the keys are edited — waiting cannot repair it.
    pub fn revoke(&self, idx: usize, reason: &str) {
        let mut st = self.lock();
        if let Some(state) = st.keys.get_mut(idx) {
            state.revoked = true;
            state.cooling_until = None;
            state.last_error = Some(reason.to_string());
        }
        if !self.keys.is_empty() {
            st.cursor = (idx + 1) % self.keys.len();
        }
    }

    /// What the status endpoint reports.
    pub fn snapshot(&self) -> PoolSnapshot {
        let st = self.lock();
        let now = Instant::now();
        let mut cooling = 0usize;
        let mut revoked = 0usize;
        let mut next: Option<Duration> = None;
        let mut last_error = None;
        for state in &st.keys {
            if state.revoked {
                revoked += 1;
                continue;
            }
            match state.cooling_until {
                Some(until) if until > now => {
                    cooling += 1;
                    let wait = until - now;
                    next = Some(next.map_or(wait, |w: Duration| w.min(wait)));
                }
                _ => {}
            }
            if last_error.is_none() {
                last_error = state.last_error.clone();
            }
        }
        PoolSnapshot {
            keys: self.keys.len(),
            cooling,
            revoked,
            // How long until a parked key comes back, whether or not others are still usable:
            // the UI renders it next to the cooling count ("1 cooling (47s)"), and a request
            // that finds every key parked needs exactly this number.
            next_available_in: next.map(|wait| wait.as_secs().max(1)).unwrap_or(0),
            last_error,
        }
    }
}

/// A pool's state at one instant. Never contains a key or any part of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSnapshot {
    pub keys: usize,
    pub cooling: usize,
    pub revoked: usize,
    /// Seconds until the earliest cooling key returns; `0` when a key is usable now.
    pub next_available_in: u64,
    pub last_error: Option<String>,
}

impl PoolSnapshot {
    /// A pool that has never been built (unconfigured section).
    pub fn unconfigured() -> Self {
        Self {
            keys: 0,
            cooling: 0,
            revoked: 0,
            next_available_in: 0,
            last_error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(20);

    fn ring(keys: &[&str]) -> Keyring {
        Keyring::new(keys.iter().map(|k| k.to_string()))
    }

    #[test]
    fn requests_rotate_across_the_keys() {
        let r = ring(&["a", "b", "c"]);
        let mut order = Vec::new();
        for _ in 0..6 {
            match r.pick(&[]) {
                Pick::Try(i) => {
                    order.push(r.keys()[i].clone());
                    r.advance(i);
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(order, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn a_cooled_key_is_skipped_and_comes_back_on_its_own() {
        let r = ring(&["a", "b", "c"]);
        r.cool(0, Some(SHORT));
        // Two requests in a row avoid the cooling key entirely.
        for _ in 0..2 {
            match r.pick(&[]) {
                Pick::Try(i) => {
                    assert_ne!(i, 0, "a cooling key must not be picked");
                    r.advance(i);
                }
                other => panic!("{other:?}"),
            }
        }
        std::thread::sleep(SHORT * 2);
        let mut saw_cooled = false;
        for _ in 0..3 {
            if let Pick::Try(i) = r.pick(&[]) {
                saw_cooled |= i == 0;
                r.advance(i);
            }
        }
        assert!(saw_cooled, "the key must return after its cooldown");
    }

    #[test]
    fn a_key_is_tried_once_per_request() {
        let r = ring(&["a", "b"]);
        r.cool(0, Some(SHORT));
        r.cool(1, Some(SHORT));
        match r.pick(&[0, 1]) {
            Pick::Nothing(Exhausted::AllCooling { wait }) => {
                // Some of the cooldown has already elapsed, so compare with a floor.
                assert!(wait <= SHORT && wait > SHORT / 2, "{wait:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_request_that_used_every_key_reports_all_tried() {
        // Neither cooling nor revoked, but already spent by this request: there is nothing
        // left to pick, and the caller reports the upstream error it already has.
        let r = ring(&["a", "b"]);
        assert_eq!(r.pick(&[0, 1]), Pick::Nothing(Exhausted::AllTried));
    }

    #[test]
    fn when_everything_is_cooling_the_wait_names_the_earliest() {
        let r = ring(&["a", "b", "c"]);
        r.cool(0, Some(Duration::from_secs(60)));
        r.cool(1, Some(Duration::from_secs(30)));
        r.cool(2, Some(Duration::from_secs(5)));
        match r.pick(&[]) {
            Pick::Nothing(Exhausted::AllCooling { wait }) => {
                assert!(wait <= Duration::from_secs(5), "{wait:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_revoked_key_never_comes_back() {
        let r = ring(&["a", "b"]);
        r.revoke(0, "401 unauthorized");
        for _ in 0..4 {
            match r.pick(&[]) {
                Pick::Try(i) => assert_eq!(i, 1),
                other => panic!("{other:?}"),
            }
            r.advance(1);
        }
        r.revoke(1, "401 unauthorized");
        assert_eq!(r.pick(&[]), Pick::Nothing(Exhausted::AllRevoked));
        // And no amount of waiting changes that.
        std::thread::sleep(SHORT);
        assert_eq!(r.pick(&[]), Pick::Nothing(Exhausted::AllRevoked));
    }

    #[test]
    fn an_empty_pool_says_so_instead_of_pretending_to_be_exhausted() {
        let r = ring(&[]);
        assert!(r.is_empty());
        assert_eq!(r.pick(&[]), Pick::Nothing(Exhausted::Empty));
        // Every operation is a no-op rather than a panic.
        r.advance(0);
        r.cool(0, None);
        r.revoke(0, "x");
        assert_eq!(r.snapshot(), PoolSnapshot::unconfigured());
    }

    #[test]
    fn a_single_key_pool_is_the_shape_that_existed_before() {
        let r = Keyring::single("only");
        assert_eq!(r.keys(), ["only".to_string()]);
        assert_eq!(r.pick(&[]), Pick::Try(0));
        r.advance(0);
        assert_eq!(r.pick(&[]), Pick::Try(0));
    }

    #[test]
    fn blanks_are_dropped_and_duplicates_do_not_get_a_second_share() {
        // A pasted list with the same key twice would otherwise spend it twice as often.
        let r = Keyring::new(["a", "  ", "a ", "b", "b"].iter().map(|s| s.to_string()));
        assert_eq!(r.keys(), ["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn the_cap_limits_how_many_keys_a_paste_can_add() {
        let many: Vec<String> = (0..MAX_KEYS + 5).map(|i| format!("k{i}")).collect();
        assert_eq!(Keyring::new(many).keys().len(), MAX_KEYS);
    }

    #[test]
    fn the_snapshot_counts_states_without_ever_leaking_a_key() {
        let r = ring(&["secret-a", "secret-b", "secret-c"]);
        r.cool(0, Some(Duration::from_secs(42)));
        r.revoke(1, "403 forbidden");
        r.cool(2, Some(Duration::from_secs(90)));
        let snap = r.snapshot();
        assert_eq!(snap.keys, 3);
        assert_eq!(snap.cooling, 2);
        assert_eq!(snap.revoked, 1);
        // Nothing is usable, so the wait is until the earliest cooling key returns.
        assert!((41..=42).contains(&snap.next_available_in), "{snap:?}");
        let rendered = format!("{snap:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn the_wait_names_the_earliest_parked_key_even_when_others_are_usable() {
        // The UI shows this number beside the cooling count, so it describes the parked key
        // rather than answering "can anything be spent right now".
        let r = ring(&["a", "b"]);
        r.cool(0, Some(Duration::from_secs(30)));
        let snap = r.snapshot();
        assert_eq!(snap.cooling, 1);
        assert!((29..=30).contains(&snap.next_available_in), "{snap:?}");
    }

    #[test]
    fn a_success_clears_that_keys_last_error_but_not_the_others() {
        let r = ring(&["a", "b"]);
        r.cool(0, Some(SHORT));
        r.cool(1, Some(SHORT));
        r.advance(1);
        assert_eq!(r.snapshot().last_error.as_deref(), Some("429 rate limited"));
    }

    #[test]
    fn a_single_key_pool_reports_itself_as_single() {
        assert!(Keyring::single("only").is_single());
        assert!(!ring(&["a", "b"]).is_single());
        assert!(ring(&[]).is_single(), "an empty pool cannot rotate either");
    }

    #[test]
    fn refusals_are_listed_without_the_keys() {
        let r = ring(&["secret-a", "secret-b"]);
        r.revoke(1, "401 unauthorized");
        assert_eq!(r.refusals(), vec![(1, "401 unauthorized".to_string())]);
    }

    #[test]
    fn noting_a_failure_does_not_take_the_key_out_of_circulation() {
        let r = Keyring::single("only");
        r.note_failure(0, "429 rate limited");
        assert_eq!(r.pick(&[]), Pick::Try(0), "a single key stays available");
        assert_eq!(r.snapshot().cooling, 0);
        assert_eq!(r.snapshot().last_error.as_deref(), Some("429 rate limited"));
    }

    #[test]
    fn exhausted_messages_never_mention_a_key() {
        for e in [
            Exhausted::Empty,
            Exhausted::AllRevoked,
            Exhausted::AllTried,
            Exhausted::AllCooling {
                wait: Duration::from_secs(47),
            },
        ] {
            let m = e.message();
            assert!(!m.is_empty());
            assert!(!m.contains("sk-"), "{m}");
        }
        assert!(Exhausted::AllCooling {
            wait: Duration::from_secs(47)
        }
        .message()
        .contains("47s"));
    }
}
