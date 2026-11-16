//! Remember which texts failed to embed during one `add`.
//!
//! `Memory::add` embeds the extracted entries and then silently drops the ones that have no
//! vector: `embed_batch` fails as a whole, the fork retries each entry on its own, and an
//! entry whose retry fails is skipped — the add still returns the entries that did embed, so
//! the loss is invisible from outside (fewer results, no error, no reason). That is the third
//! failure family the retry queue exists for, and this is how the app learns about it without
//! changing the fork.
//!
//! A [`RecordingEmbedder`] wraps the section's embedder and notes every text whose `embed`
//! returned an error. Callers bracket an add the same way extraction diagnostics are bracketed:
//!
//! ```ignore
//! let before = crate::embed_log::current_seq();
//! let results = mem.add(messages, opts).await?;
//! let dropped = crate::embed_log::since(before);
//! ```
//!
//! Bracketing by sequence rather than by time keeps concurrent adds from reading each other's
//! records in the common case. A window is not airtight: a search whose query could not be
//! embedded while an add was running lands in that add's window too. The consequence is
//! bounded but real — the query text can end up in the failed-add stash, so a retry would
//! store it as a memory. That is one wrong entry in a rare overlap, versus the alternative of
//! no signal at all, which is what "the entry was silently dropped" looked like before.

use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use mem0::{Embedder, MemoryAction};
use mem0_core::error::Result;

/// How many records to keep. One add records at most one entry per extracted memory and the
/// caller drains them immediately, so this only has to absorb bursts.
const CAPACITY: usize = 64;

struct Store {
    seq: u64,
    records: Vec<(u64, String)>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(Store {
            seq: 0,
            records: Vec::new(),
        })
    })
}

/// Note a text that could not be embedded.
pub fn record(text: &str) {
    let Ok(mut s) = store().lock() else {
        // A poisoned lock must not take down an add: this is bookkeeping.
        return;
    };
    s.seq += 1;
    let seq = s.seq;
    if s.records.len() >= CAPACITY {
        s.records.remove(0);
    }
    s.records.push((seq, text.to_string()));
}

/// The sequence number to bracket an `add` with.
pub fn current_seq() -> u64 {
    store().lock().map(|s| s.seq).unwrap_or(0)
}

/// The distinct texts recorded after `seq`, oldest first.
pub fn since(seq: u64) -> Vec<String> {
    let Ok(s) = store().lock() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for (_, text) in s.records.iter().filter(|(at, _)| *at > seq) {
        if !out.contains(text) {
            out.push(text.clone());
        }
    }
    out
}

/// Delegating embedder that notes the texts it could not embed.
pub struct RecordingEmbedder {
    inner: Box<dyn Embedder>,
}

impl RecordingEmbedder {
    pub fn new(inner: Box<dyn Embedder>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    async fn embed(&self, text: &str, action: MemoryAction) -> Result<Vec<f32>> {
        match self.inner.embed(text, action).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // Only the per-call path records: `add` retries a failed batch entry by
                // entry, so recording there is exactly the set of texts that were lost.
                record(text);
                Err(e)
            }
        }
    }

    async fn embed_batch(&self, texts: &[String], action: MemoryAction) -> Result<Vec<Vec<f32>>> {
        // Deliberately silent: the caller retries entry by entry, and those calls are what
        // get recorded. Recording here too would note entries that went on to succeed.
        self.inner.embed_batch(texts, action).await
    }

    fn dims(&self) -> usize {
        self.inner.dims()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Failing;

    #[async_trait]
    impl Embedder for Failing {
        async fn embed(&self, text: &str, _action: MemoryAction) -> Result<Vec<f32>> {
            if text.contains("bad") {
                Err(mem0_core::error::Mem0Error::embedding("nope"))
            } else {
                Ok(vec![0.0; 3])
            }
        }

        fn dims(&self) -> usize {
            3
        }
    }

    #[tokio::test]
    async fn only_failures_are_recorded_and_only_within_the_window() {
        let e = RecordingEmbedder::new(Box::new(Failing));
        let before = current_seq();
        e.embed("good", MemoryAction::Add).await.unwrap();
        assert!(e.embed("bad one", MemoryAction::Add).await.is_err());
        assert!(e.embed("bad one", MemoryAction::Add).await.is_err());
        let seen = since(before);
        assert_eq!(
            seen,
            vec!["bad one".to_string()],
            "deduplicated, failures only"
        );

        let after = current_seq();
        assert!(since(after).is_empty(), "a fresh window starts empty");
    }
}
