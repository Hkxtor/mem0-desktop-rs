//! Classify a memory after the add that stored it has already been answered.
//!
//! Classification is best-effort by design: a failure leaves `categories: []` and never fails
//! the add. Waiting for it inside the request made the caller pay for work it does not consume —
//! a real add measured 11017 ms of which 1383 ms was classification, and the tags only ever
//! surface on the Memories and Categories pages, not in the add response.
//!
//! So the ids go into a bounded channel and one worker drains it, one memory at a time. Serial
//! on purpose: the classifier spends the same provider keys the extraction just used, and the
//! add that queued six memories should not turn into six concurrent model calls afterwards.
//!
//! What is deliberately *not* here: persistence. A queue lost to a crash shows up as unclassified
//! memories, which the "分类 N 条未分类" button and `POST /systemone/classify-batch` already fix.
//! A durable queue would be a second, quieter copy of the truth.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::http::AppState;

/// How many memories may wait to be classified before new ones are dropped.
///
/// 256 is two orders of magnitude above a normal add rate (one add every few seconds, six
/// memories each) and still bounds memory: an id is a few dozen bytes. Dropping is survivable
/// precisely because the unclassified counter is the fallback path.
pub const QUEUE_CAPACITY: usize = 256;

/// The app-side half of the queue: everything a request needs to hand work off.
///
/// The receiving half lives in the worker, so a caller can enqueue without being able to
/// interfere with it.
pub struct ClassifyQueue {
    tx: mpsc::Sender<String>,
    pending: Arc<AtomicUsize>,
    classified: Arc<AtomicUsize>,
}

impl ClassifyQueue {
    /// A queue nobody consumes, for tests that build an `AppState` without a running worker.
    pub fn disconnected() -> Self {
        let (tx, _rx) = mpsc::channel(QUEUE_CAPACITY);
        Self {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            classified: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Split a queue into the side callers hold and the side the worker drains.
    pub fn channel() -> (Self, Worker) {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let pending = Arc::new(AtomicUsize::new(0));
        let classified = Arc::new(AtomicUsize::new(0));
        (
            Self {
                tx,
                pending: pending.clone(),
                classified: classified.clone(),
            },
            Worker {
                rx,
                pending,
                classified,
            },
        )
    }

    /// Hand over the ids of memories that still need a category. Returns how many were dropped.
    ///
    /// `try_send` on purpose: a full queue must not make the add wait. The caller logs the loss.
    pub fn enqueue(&self, ids: &[String]) -> usize {
        let mut dropped = 0;
        for id in ids {
            match self.tx.try_send(id.clone()) {
                Ok(()) => {
                    self.pending.fetch_add(1, Ordering::SeqCst);
                }
                Err(_) => dropped += 1,
            }
        }
        dropped
    }

    /// Memories waiting to be classified right now.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Memories this process classified in the background since it started.
    pub fn classified(&self) -> usize {
        self.classified.load(Ordering::SeqCst)
    }
}

/// The worker's half. Handed to [`run`] exactly once, at startup.
pub struct Worker {
    rx: mpsc::Receiver<String>,
    pending: Arc<AtomicUsize>,
    classified: Arc<AtomicUsize>,
}

impl Worker {
    /// Memories waiting to be classified right now.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
}

/// One memory's classification, from an id to the tags on its row.
///
/// Every failure mode is a log line and a `continue`: a wrong base_url, a rejected key or a
/// model that answers nonsense must not stop the queue, and must not be retried in a loop
/// (`/systemone/counts.last_error` plus the unclassified count are what tell the operator).
async fn classify_one(state: &Arc<AppState>, id: &str) {
    let mem = match crate::http::routes_config::memory_or_restore(state).await {
        Some(m) => m,
        // No runtime configured (yet). The memory is not classifiable now; leave it.
        None => return,
    };
    let classifier = match crate::http::classifier_or_restore(state).await {
        Some(c) => c,
        // Classification is off. The ids queued before the operator turned it off are moot —
        // and re-reading the config per id would spin.
        None => return,
    };
    // The row is the source of truth for what to classify: the payload may have been edited
    // since the add, and reading it also tells us the memory still exists.
    let text = match mem.vector_store().get(id).await {
        Ok(Some(hit)) => hit
            .payload
            .get("data")
            .or_else(|| hit.payload.get("memory"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        Ok(None) => return,
        Err(e) => {
            crate::logging::warn(
                "systemone",
                "could not read a queued memory",
                serde_json::json!({"id": id, "error": e.to_string()}),
            );
            return;
        }
    };
    if text.trim().is_empty() {
        return;
    }
    let categories =
        match crate::http::routes_systemone::classify_recorded(state, &classifier, &text).await {
            Ok(cats) => cats,
            Err(e) => {
                // `classify_recorded` already recorded the failure for /systemone/counts; one line
                // here so the log shows which memory it was about.
                crate::logging::warn(
                    "systemone",
                    "background classification failed",
                    serde_json::json!({"id": id, "error": e}),
                );
                return;
            }
        };
    if categories.is_empty() {
        // "No preset category applies" is an answer, not a failure.
        return;
    }
    if let Err(e) = crate::http::routes_systemone::write_categories(&mem, id, &categories).await {
        crate::logging::warn(
            "systemone",
            "background classification could not be written",
            serde_json::json!({"id": id, "error": e.to_string()}),
        );
    }
}

/// Drain the queue until the process ends.
///
/// Never returns on its own and never panics out: the queue ending means the sender was
/// dropped, which only happens at shutdown.
pub async fn run(state: Arc<AppState>, mut worker: Worker) {
    while let Some(id) = worker.rx.recv().await {
        worker.pending.fetch_sub(1, Ordering::SeqCst);
        classify_one(&state, &id).await;
        worker.classified.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_drops_the_rest_instead_of_waiting() {
        let (queue, _worker) = ClassifyQueue::channel();
        let ids: Vec<String> = (0..QUEUE_CAPACITY + 5).map(|i| format!("id-{i}")).collect();
        let dropped = queue.enqueue(&ids);
        assert_eq!(dropped, 5, "the queue is bounded at {QUEUE_CAPACITY}");
        assert_eq!(queue.pending(), QUEUE_CAPACITY);
    }

    #[test]
    fn a_disconnected_queue_does_not_panic_the_add() {
        // What a test-only `AppState` looks like: nobody is listening, and enqueueing five
        // memories must still be a no-op rather than a panic in the write path.
        let queue = ClassifyQueue::disconnected();
        assert_eq!(queue.enqueue(&["a".into(), "b".into()]), 2);
        assert_eq!(queue.pending(), 0);
    }

    #[tokio::test]
    async fn the_worker_leaves_when_the_queue_closes() {
        let (queue, worker) = ClassifyQueue::channel();
        let dir = tempfile::tempdir().expect("tempdir");
        let state = Arc::new(crate::http::AppState::new(dir.path().to_path_buf()).expect("state"));
        let handle = tokio::spawn(run(state, worker));
        drop(queue);
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("worker should stop when the sender is gone")
            .expect("no panic");
    }
}
