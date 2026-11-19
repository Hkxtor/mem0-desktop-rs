//! Recording what happened to the memories, for the dashboard's Requests page.
//!
//! That page answers "what did clients do with my memories" — added, searched, changed,
//! deleted — not "which HTTP routes were hit". The two are different questions: an HTTP
//! log is dominated by `/configure`, `/auth/refresh` and dashboard polling, while the
//! memory actions are what an operator opens the page for.

use std::sync::Arc;
use std::time::Instant;

use axum::response::Json;
use serde_json::{json, Value};

use super::error::ApiError;
use super::AppState;

/// Render a scope (`user:alice · agent:support`) for the detail column.
pub fn scope_detail(user: Option<&str>, agent: Option<&str>, run: Option<&str>) -> String {
    let mut parts = Vec::new();
    for (label, value) in [("user", user), ("agent", agent), ("run", run)] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            parts.push(format!("{label}:{v}"));
        }
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Number of entries in a `{"results": [...]}` payload.
pub fn count_results(payload: &Value) -> Option<i64> {
    payload
        .get("results")
        .and_then(|r| r.as_array())
        .map(|a| a.len() as i64)
}

/// Number of entries a `result_summary` keeps.
///
/// A search with `top_k: 1000` would otherwise put the whole store into one log row, and the
/// column exists so a row can be recognized later, not so the store can be replayed from it.
const SUMMARY_MAX_ITEMS: usize = 20;

/// Longest memory text kept per entry, in **characters** (not bytes): a Chinese memory cut at 80
/// bytes would lose two thirds of the sentence it needs to be recognizable.
const SUMMARY_TEXT_CHARS: usize = 80;

/// What a row stored or found, abbreviated, in the shape the Requests page expands.
///
/// `{"items": [{"id", "memory", "score"?}], "total": n, "truncated": bool}`. `None` for the
/// operations with nothing per-item to say: `IMPORT` already names its count in `detail`, and
/// twenty abbreviated memories from a bulk load say less than the number does.
///
/// Failure detail is not repeated here — `reason` and `error_kind` answer that, and a failed add
/// that kept its input is reachable through `failed_add_id`.
pub fn summarize(operation: &str, detail: &str, payload: &Value) -> Option<Value> {
    match operation {
        // `GET /memories/:id` answers with the memory itself, not a `results` array.
        "GET" => summarize_one(detail, payload),
        "ADD" | "SEARCH" => summarize_many(operation, payload),
        // The row already names the id in `detail`; the summary exists so the expand can offer a
        // jump back to the memory rather than showing a bare string.
        "UPDATE" | "DELETE" => Some(json!({
            "items": [{ "id": detail }],
            "total": 1,
            "truncated": false,
        })),
        _ => None,
    }
}

/// The `{"results": [...]}` operations: an add's stored entries, a search's hits.
fn summarize_many(operation: &str, payload: &Value) -> Option<Value> {
    let results = payload.get("results").and_then(|r| r.as_array())?;
    if results.is_empty() {
        // Nothing to expand. An empty result with a `reason` explains itself on the row.
        return None;
    }
    let items: Vec<Value> = results
        .iter()
        .take(SUMMARY_MAX_ITEMS)
        .map(|r| {
            let mut item = serde_json::Map::new();
            if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                item.insert("id".into(), json!(id));
            }
            item.insert(
                "memory".into(),
                json!(abbreviate(
                    r.get("memory").and_then(|v| v.as_str()).unwrap_or("")
                )),
            );
            if operation == "SEARCH" {
                // Three decimals: enough to tell a good hit from a near miss, without the
                // 17-digit tail of a float nobody can read.
                if let Some(score) = r.get("score").and_then(|v| v.as_f64()) {
                    item.insert("score".into(), json!((score * 1000.0).round() / 1000.0));
                }
            }
            Value::Object(item)
        })
        .collect();
    Some(json!({
        "items": items,
        "total": results.len(),
        "truncated": results.len() > SUMMARY_MAX_ITEMS,
    }))
}

/// `GET /memories/:id` answers with the memory itself, not a `results` array.
fn summarize_one(id: &str, payload: &Value) -> Option<Value> {
    let text = payload
        .get("memory")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("data").and_then(|v| v.as_str()))
        .unwrap_or("");
    Some(json!({
        "items": [{ "id": id, "memory": abbreviate(text) }],
        "total": 1,
        "truncated": false,
    }))
}

/// The first [`SUMMARY_TEXT_CHARS`] characters of `text`, with an ellipsis when cut.
fn abbreviate(text: &str) -> String {
    if text.chars().count() <= SUMMARY_TEXT_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(SUMMARY_TEXT_CHARS).collect();
    format!("{head}…")
}

/// Operations whose payload is worth abbreviating at all.
///
/// Kept as a check so `IMPORT` and `RESET` do not pay for reading a payload nobody expands.
fn has_detail(operation: &str) -> bool {
    matches!(operation, "ADD" | "SEARCH" | "GET" | "UPDATE" | "DELETE")
}

/// A handler payload the Requests page can expand.
///
/// A trait rather than a `serde::Serialize` bound because `axum::Json` is not `Serialize`, and
/// rather than another closure argument because the body of every recorded handler answers with
/// the same `Json<Value>` — one impl covers them all and no call site has to mention it.
pub trait RecordedPayload {
    /// The JSON body, when there is one to abbreviate.
    fn as_value(&self) -> Option<&Value>;
}

impl RecordedPayload for Json<Value> {
    fn as_value(&self) -> Option<&Value> {
        Some(&self.0)
    }
}

/// Handlers that record an operation but answer with nothing worth expanding.
impl RecordedPayload for () {
    fn as_value(&self) -> Option<&Value> {
        None
    }
}

/// Write the row. Takes the extras as one struct so the signature does not grow with every
/// annotation the Requests page learns to show.
fn persist(
    state: &AppState,
    operation: &str,
    detail: &str,
    results: Option<i64>,
    status: i64,
    latency_ms: f64,
    extras: &crate::store::OpExtras<'_>,
) {
    // Logging must never turn a successful memory call into an error: this is bookkeeping.
    if let Ok(store) = state.store.lock() {
        if let Err(e) = store
            .log_memory_operation_with_kind(operation, detail, results, status, latency_ms, extras)
        {
            eprintln!("failed to record memory operation: {e}");
        }
    }
}

/// What an operation wants the Requests page to say about it.
///
/// `reason` explains a zero the operator cannot otherwise decode; `failed_add_id` points at
/// the stash holding the input, so the row can offer a retry; `error_kind` says what kind of
/// upstream failure caused it, which is what turns "模型调用失败" into advice.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub reason: Option<String>,
    pub failed_add_id: Option<i64>,
    pub error_kind: Option<String>,
}

/// Run a handler body and record the outcome either way.
///
/// `results_of` extracts the count from a successful payload, so an ADD can report how
/// many memories it produced and a SEARCH how many it returned.
pub async fn record<T, F, Fut, C>(
    state: Arc<AppState>,
    operation: &'static str,
    detail: String,
    started: Instant,
    results_of: C,
    body: F,
) -> Result<T, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
    C: Fn(&T) -> Option<i64>,
    T: RecordedPayload,
{
    record_with_reason(
        state,
        operation,
        detail,
        started,
        results_of,
        |_| None,
        body,
    )
    .await
}

/// [`record`], plus a short explanation of an otherwise inexplicable result.
///
/// Only `ADD` needs it today: a 200 with zero results is either "the message said nothing
/// worth remembering" or "the model returned an error the pipeline swallowed", and the
/// Requests page should not make the operator guess which.
pub async fn record_with_reason<T, F, Fut, C, R>(
    state: Arc<AppState>,
    operation: &'static str,
    detail: String,
    started: Instant,
    results_of: C,
    reason_of: R,
    body: F,
) -> Result<T, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
    C: Fn(&T) -> Option<i64>,
    R: Fn(&Result<T, ApiError>) -> Option<Outcome>,
    T: RecordedPayload,
{
    // Measured, not merely timed: the counter covers this body, and the delegating clients below
    // it record which stage each wait belonged to. `latency_ms` stays the operator's clock and
    // the stages are a breakdown of it, not a second opinion.
    let (result, stages) = crate::stage_timing::measure(body()).await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    // The closure sees failures too: an add that threw still knows why, and may have kept the
    // input it could not process.
    let outcome = reason_of(&result);
    // Only the operations the Requests page can expand are worth abbreviating: an `IMPORT`
    // payload is the whole imported set and nobody reads it back here.
    let summary = match &result {
        Ok(payload) if has_detail(operation) => payload
            .as_value()
            .and_then(|v| summarize(operation, &detail, v))
            .map(|v| v.to_string()),
        _ => None,
    };
    let stages = stages.map(|v| v.to_string());
    let extras = crate::store::OpExtras {
        reason: outcome.as_ref().and_then(|o| o.reason.as_deref()),
        failed_add_id: outcome.as_ref().and_then(|o| o.failed_add_id),
        error_kind: outcome.as_ref().and_then(|o| o.error_kind.as_deref()),
        result_summary: summary.as_deref(),
        stages: stages.as_deref(),
    };
    match &result {
        Ok(payload) => persist(
            &state,
            operation,
            &detail,
            results_of(payload),
            200,
            latency_ms,
            &extras,
        ),
        Err(e) => persist(
            &state,
            operation,
            &detail,
            None,
            e.status.as_u16() as i64,
            latency_ms,
            &extras,
        ),
    }
    result
}

/// Record an operation that does not return a payload worth counting.
pub async fn record_unit<F, Fut>(
    state: Arc<AppState>,
    operation: &'static str,
    detail: String,
    started: Instant,
    results: Option<i64>,
    body: F,
) -> Result<Json<Value>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Json<Value>, ApiError>>,
{
    record(state, operation, detail, started, move |_| results, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_payload(texts: &[&str]) -> Value {
        json!({
            "results": texts.iter().map(|t| json!({"id": format!("id-{t}"), "memory": t})).collect::<Vec<_>>()
        })
    }

    #[test]
    fn an_add_keeps_the_entries_it_stored() {
        let summary = summarize(
            "ADD",
            "user:u1",
            &add_payload(&["likes tea", "runs on Sundays"]),
        )
        .expect("an add with results has detail");
        assert_eq!(summary["total"], 2);
        assert_eq!(summary["truncated"], false);
        assert_eq!(summary["items"][0]["memory"], "likes tea");
        assert_eq!(summary["items"][1]["id"], "id-runs on Sundays");
    }

    #[test]
    fn a_search_keeps_the_score_and_an_add_does_not_invent_one() {
        let hits = json!({"results": [{"id": "a", "memory": "tea", "score": 0.5123456}]});
        let search = summarize("SEARCH", "\"tea\" · user:u1", &hits).unwrap();
        assert_eq!(search["items"][0]["score"], 0.512);
        let add = summarize("ADD", "user:u1", &hits).unwrap();
        assert!(
            add["items"][0].get("score").is_none(),
            "an add result carries no score, so the summary must not invent one"
        );
    }

    #[test]
    fn an_operation_with_nothing_to_show_gets_no_summary() {
        // The bulk load already states its count in `detail`; twenty abbreviated memories say
        // less than the number does.
        assert!(summarize("IMPORT", "1200 memories", &add_payload(&["x"])).is_none());
        assert!(summarize("RESET", "all", &json!({})).is_none());
        // An add that stored nothing explains itself through `reason`, not through an empty list.
        assert!(summarize("ADD", "user:u1", &json!({"results": []})).is_none());
    }

    #[test]
    fn the_summary_is_bounded_in_both_length_and_count() {
        let long = "记".repeat(200);
        let texts: Vec<&str> = (0..30).map(|_| long.as_str()).collect();
        let summary = summarize("ADD", "user:u1", &add_payload(&texts)).unwrap();
        let items = summary["items"].as_array().unwrap();
        assert_eq!(items.len(), SUMMARY_MAX_ITEMS);
        assert_eq!(summary["total"], 30);
        assert_eq!(summary["truncated"], true);
        // Cut by characters, not bytes: 80 Chinese characters plus the ellipsis.
        let kept = items[0]["memory"].as_str().unwrap();
        assert_eq!(kept.chars().count(), SUMMARY_TEXT_CHARS + 1);
        assert!(kept.ends_with('…'));
    }

    #[test]
    fn a_short_text_is_kept_whole() {
        let summary = summarize("ADD", "user:u1", &add_payload(&["teacakes on Sunday"])).unwrap();
        assert_eq!(summary["items"][0]["memory"], "teacakes on Sunday");
        assert!(!summary["items"][0]["memory"]
            .as_str()
            .unwrap()
            .contains('…'));
    }

    #[test]
    fn a_single_memory_read_names_the_id_it_was_asked_for() {
        let summary = summarize(
            "GET",
            "mem-1",
            &json!({"id": "mem-1", "memory": "likes tea"}),
        )
        .expect("a read has one memory to show");
        assert_eq!(summary["items"][0]["id"], "mem-1");
        assert_eq!(summary["items"][0]["memory"], "likes tea");
    }

    #[test]
    fn a_change_points_at_the_memory_it_touched() {
        // The payload is only a confirmation sentence; the id is what the expand can jump to.
        for op in ["UPDATE", "DELETE"] {
            let summary = summarize(op, "mem-9", &json!({"message": "ok"})).unwrap();
            assert_eq!(summary["items"][0]["id"], "mem-9");
            assert_eq!(summary["truncated"], false);
        }
    }

    #[test]
    fn every_expandable_operation_is_one_summarize_answers_for() {
        for op in ["ADD", "SEARCH", "GET", "UPDATE", "DELETE"] {
            assert!(has_detail(op), "{op} should carry detail");
        }
        for op in ["IMPORT", "RESET", "DELETE_ALL", "LIST"] {
            assert!(!has_detail(op), "{op} should not carry detail");
        }
    }
}
