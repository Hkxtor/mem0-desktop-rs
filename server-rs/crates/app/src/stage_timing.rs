//! Where an operation's time went.
//!
//! The Requests page already showed a total, which answers "was that slow?" and not "slow where?".
//! An add spends its time in three places that fail for different reasons — the model extracting,
//! the embedder vectorizing, the classifier tagging — and the retry advice differs for each
//! (add keys, pick a faster embedder, turn classification off).
//!
//! Timing happens *above* the clients, in delegating wrappers, so the fork needs no changes and
//! every wrapper already in the chain (key pooling, the failed-embed log) keeps working.
//!
//! # Why a task-local
//!
//! The wrapped clients are built once per configuration and live inside `Memory` for the life of
//! the process, so they cannot hold per-request state. A task-local counter is created by
//! [`measure`] around the handler body, so it is per-request by construction and gone when the
//! request is.
//!
//! Spawned tasks do **not** inherit it, and that is now the whole story for classification:
//! tagging moved to a background worker ([`crate::classify_queue`]), so it does not happen
//! inside any request and records nothing here. `classify_ms` stays in the shape at 0 — the
//! Requests page keeps its four segments, and dropping the key would move every golden file.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mem0::{Embedder, GenerateOptions, Llm, MemoryAction, Message, Result};
use serde_json::{json, Value};

/// Time attributed to each stage of one operation, in milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct StageTimers {
    pub llm_ms: f64,
    pub embed_ms: f64,
    pub classify_ms: f64,
}

tokio::task_local! {
    /// The counter of the operation currently running on this task, when it is being measured.
    static STAGES: Arc<Mutex<StageTimers>>;
}

fn millis(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Add to the current operation's counter, if it is being measured.
///
/// Silent when it is not: a retry loop and a spawned classify task both drive the same clients
/// outside any measured scope, and bookkeeping must never fail a call.
fn add(f: impl FnOnce(&mut StageTimers)) {
    let _ = STAGES.try_with(|timers| {
        if let Ok(mut t) = timers.lock() {
            f(&mut t);
        }
    });
}

/// Note time spent waiting on the model.
pub fn add_llm_ms(ms: f64) {
    add(|t| t.llm_ms += ms);
}

/// Note time spent waiting on the embedder.
pub fn add_embed_ms(ms: f64) {
    add(|t| t.embed_ms += ms);
}

/// Run an operation body with a fresh counter and report where its time went.
///
/// The returned `stages` is JSON ready for the column, or `None` when nothing was recorded — a
/// read or a delete never calls a model, and a breakdown that is entirely "other" says less than
/// the total already does.
///
/// `other_ms` is what is left of this body's own wall clock: the vector store's insert, the
/// history write, and the mutex waits between them. It is clamped at zero because the pieces are
/// measured on slightly different clocks and rounding can make the sum exceed the whole by a
/// fraction of a millisecond.
pub async fn measure<F, T>(body: F) -> (T, Option<Value>)
where
    F: Future<Output = T>,
{
    let timers = Arc::new(Mutex::new(StageTimers::default()));
    let started = Instant::now();
    let out = STAGES.scope(timers.clone(), body).await;
    let total_ms = millis(started);
    let stages = timers.lock().map(|t| *t).unwrap_or_default();
    if stages.llm_ms == 0.0 && stages.embed_ms == 0.0 && stages.classify_ms == 0.0 {
        return (out, None);
    }
    let other_ms = (total_ms - stages.llm_ms - stages.embed_ms - stages.classify_ms).max(0.0);
    (
        out,
        Some(json!({
            "llm_ms": round(stages.llm_ms),
            "embed_ms": round(stages.embed_ms),
            "classify_ms": round(stages.classify_ms),
            "other_ms": round(other_ms),
        })),
    )
}

/// One decimal: milliseconds with more precision than that are noise from the clock, and the
/// column is read by a person.
fn round(ms: f64) -> f64 {
    (ms * 10.0).round() / 10.0
}

/// A model client that notes how long each call took.
pub struct TimingLlm {
    inner: Box<dyn Llm>,
}

impl TimingLlm {
    pub fn new(inner: Box<dyn Llm>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl Llm for TimingLlm {
    async fn generate(&self, messages: &[Message], options: &GenerateOptions) -> Result<String> {
        let started = Instant::now();
        let out = self.inner.generate(messages, options).await;
        // Counted whether it worked or not: time spent on a call that then failed is still time
        // the operator waited.
        add_llm_ms(millis(started));
        out
    }
}

/// An embedder that notes how long each call took.
pub struct TimingEmbedder {
    inner: Box<dyn Embedder>,
}

impl TimingEmbedder {
    pub fn new(inner: Box<dyn Embedder>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl Embedder for TimingEmbedder {
    async fn embed(&self, text: &str, action: MemoryAction) -> Result<Vec<f32>> {
        let started = Instant::now();
        let out = self.inner.embed(text, action).await;
        add_embed_ms(millis(started));
        out
    }

    /// Forwarded, not inherited: the trait's default loops over [`Embedder::embed`], which would
    /// turn one batched request back into one request per entry (see `pooled::PooledEmbedder`,
    /// which overrides it for the same reason).
    async fn embed_batch(&self, texts: &[String], action: MemoryAction) -> Result<Vec<Vec<f32>>> {
        let started = Instant::now();
        let out = self.inner.embed_batch(texts, action).await;
        add_embed_ms(millis(started));
        out
    }

    fn dims(&self) -> usize {
        self.inner.dims()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stands in for a slow provider without needing a server.
    fn sleep_ms(ms: u64) -> impl Future<Output = ()> {
        tokio::time::sleep(std::time::Duration::from_millis(ms))
    }

    #[tokio::test]
    async fn a_measured_body_reports_the_stage_it_spent_time_in() {
        let (_, stages) = measure(async {
            let started = Instant::now();
            sleep_ms(60).await;
            add_llm_ms(millis(started));
            let started = Instant::now();
            sleep_ms(30).await;
            add_embed_ms(millis(started));
        })
        .await;
        let stages = stages.expect("two stages were recorded");
        assert!(stages["llm_ms"].as_f64().unwrap() >= 55.0, "{stages}");
        assert!(stages["embed_ms"].as_f64().unwrap() >= 25.0, "{stages}");
        assert_eq!(stages["classify_ms"], 0.0);
        assert!(stages["other_ms"].as_f64().unwrap() >= 0.0, "{stages}");
    }

    #[tokio::test]
    async fn a_body_that_never_calls_a_model_reports_nothing() {
        let (out, stages) = measure(async {
            sleep_ms(5).await;
            7
        })
        .await;
        assert_eq!(out, 7);
        assert!(
            stages.is_none(),
            "a breakdown of all-other says less than the total already does"
        );
    }

    #[tokio::test]
    async fn recording_outside_a_measured_body_is_silently_dropped() {
        // No panic, no counter: the failed-add retry loop and the background classification
        // worker call the same wrapped clients with nothing measuring them.
        add_llm_ms(100.0);
        add_embed_ms(100.0);
    }

    #[tokio::test]
    async fn two_operations_on_two_tasks_do_not_share_a_counter() {
        // The whole point of a task-local: both bodies sleep the same amount, and each must
        // report its own time rather than the sum of the two.
        let calls = Arc::new(AtomicUsize::new(0));
        let one = {
            let calls = calls.clone();
            tokio::spawn(async move {
                measure(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let started = Instant::now();
                    sleep_ms(50).await;
                    add_llm_ms(millis(started));
                })
                .await
                .1
            })
        };
        let two = {
            let calls = calls.clone();
            tokio::spawn(async move {
                measure(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let started = Instant::now();
                    sleep_ms(50).await;
                    add_llm_ms(millis(started));
                })
                .await
                .1
            })
        };
        let (a, b) = (one.await.unwrap().unwrap(), two.await.unwrap().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        for stages in [&a, &b] {
            let llm = stages["llm_ms"].as_f64().unwrap();
            assert!(llm >= 45.0, "{stages}");
            assert!(
                llm < 100.0,
                "each operation must report only its own call, not both: {stages}"
            );
        }
    }
}
