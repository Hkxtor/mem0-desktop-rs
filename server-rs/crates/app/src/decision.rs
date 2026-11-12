//! The SystemOne **decision** backend: Google's `decisions` API as the classifier.
//!
//! Most `systemone` providers are OpenAI-compatible chat models: one prompt, one JSON array
//! back (see `crate::categories`). A decision API is not chat — it takes one input plus a map
//! of typed questions and returns one answer per question:
//!
//! ```text
//! POST {base_url}/v1beta/decisions
//! x-goog-api-key: <key>
//! {"model": "gemini-decision-1", "input": "<text>",
//!  "questions": {"food": {"type": "noul", "instructions": "..."}}}
//! -> {"answers": {"food": {"type": "noul", "noul": 0.886, "confidence": 0.89}}, "usage": {...}}
//! ```
//!
//! Multi-label falls out of asking one `noul` question per preset category: one request,
//! fifteen answers, several categories, no per-category round trip and no prompt to keep in
//! sync with `PRESET_CATEGORIES`.
//!
//! **`noul` is P(the condition holds)** — the category applies when that number is at or above
//! `noul_threshold`. Measured against the live endpoint: a cracked-mug message scores 0.98 on
//! "did it arrive damaged?", an intact one 0.022 on the same question. Google documents the
//! same thing ("`noul`: a yes/no check that returns the probability the condition holds"), and
//! the direction is pinned by unit tests built from real responses.
//!
//! This module never guesses: an answer that is missing, not a number, or outside 0..1 is
//! skipped rather than coerced, and a response without an `answers` object is an error
//! carrying Google's own message.

use mem0::{Mem0Error, Result};
use serde_json::{json, Value};

use crate::categories::PRESET_CATEGORIES;

/// Where the decision RPC lives. Overridable for a proxy, but there is a default because the
/// service is named: nothing is guessed about who receives the memory text.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const DEFAULT_MODEL: &str = "gemini-decision-1";

/// What each preset category means, phrased as a yes/no question for the decision API.
/// Short on purpose: the model gets fifteen of these in one request.
const INSTRUCTIONS: &[(&str, &str)] = &[
    ("personal_details", "Does this text state personal details about the person, such as their name, age, location or contact information?"),
    ("family", "Does this text concern the person's family, partner, children or relatives?"),
    ("professional_details", "Does this text concern the person's job, employer, role, career or work situation?"),
    ("sports", "Does this text concern sports, exercise, teams or athletic events?"),
    ("travel", "Does this text concern travel, trips, holidays, flights or places visited?"),
    ("food", "Does this text concern food, cooking, restaurants, drinks or eating habits?"),
    ("music", "Does this text concern music, songs, artists, instruments or concerts?"),
    ("health", "Does this text concern the person's health, medical conditions, medication or wellbeing?"),
    ("technology", "Does this text concern technology, software, programming, computers, devices or gadgets?"),
    ("hobbies", "Does this text concern hobbies, interests, crafts or leisure activities that are not covered by another category?"),
    ("fashion", "Does this text concern fashion, clothing, style or personal appearance?"),
    ("entertainment", "Does this text concern films, series, books, games or other entertainment?"),
    ("milestones", "Does this text describe a life milestone, such as a graduation, marriage, birth, move or major achievement?"),
    ("user_preferences", "Does this text state a preference, like, dislike or opinion of the person?"),
    ("misc", "Does this text contain a fact about the person that none of the other categories describe?"),
];

/// A decision-backend client. Built once per configuration, like the chat classifier.
pub struct DecisionClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    noul_threshold: f64,
}

impl DecisionClient {
    /// Build from the `systemone.config` block.
    pub fn from_config(cfg: &Value) -> Result<Self> {
        // The backend used to speak a second vendor's protocol, which is gone. A config that
        // still names it is rejected rather than quietly reinterpreted: its endpoint and key
        // belong to a service that is no longer reachable from here, and sending memory text
        // to whatever `base_url` says while labelling it Gemini would be worse than failing.
        if let Some(vendor) = cfg
            .get("vendor")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != "gemini")
        {
            return Err(Mem0Error::configuration(format!(
                "systemone vendor {vendor:?} is no longer supported: the decision backend \
                 only speaks Google's decisions API. Remove `vendor` (and the endpoint that \
                 belonged to it) to use the default."
            )));
        }
        let base_url = cfg
            .get("base_url")
            .or_else(|| cfg.get("systemone_base_url"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();
        let model = cfg
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MODEL)
            .to_string();
        let api_key = cfg
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let noul_threshold = cfg
            .get("noul_threshold")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5);
        if !(0.0..=1.0).contains(&noul_threshold) {
            return Err(Mem0Error::configuration(
                "systemone (systemone): noul_threshold must be between 0 and 1",
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| Mem0Error::configuration(format!("systemone (systemone): {e}")))?;
        Ok(Self {
            http,
            base_url,
            api_key,
            model,
            noul_threshold,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The model in effect, including the default when the config left it blank.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Where the request actually goes. Accepting a base URL that already ends at the RPC
    /// keeps "paste the full endpoint" from producing `/v1beta/decisions/v1beta/decisions`.
    fn endpoint(&self) -> String {
        if self.base_url.ends_with("/decisions") {
            self.base_url.clone()
        } else if self.base_url.ends_with("/v1beta") {
            format!("{}/decisions", self.base_url)
        } else {
            format!("{}/v1beta/decisions", self.base_url)
        }
    }

    /// The request body: one `noul` question per preset category.
    pub fn request_body(&self, text: &str) -> Value {
        let mut questions = serde_json::Map::new();
        for (name, instructions) in INSTRUCTIONS {
            questions.insert(
                (*name).to_string(),
                json!({"type": "noul", "instructions": instructions}),
            );
        }
        // Exactly these three fields: the API answers 400 for anything it does not know.
        json!({
            "model": self.model,
            "input": text,
            "questions": Value::Object(questions),
        })
    }

    /// Classify, returning both the categories and the raw answer map. The raw map is what
    /// `/systemone/test` shows, so a mis-tuned threshold is diagnosable without server logs.
    pub async fn classify_detailed(&self, text: &str) -> Result<(Vec<String>, Value)> {
        let body = self.request_body(text);
        let endpoint = self.endpoint();
        let mut req = self.http.post(&endpoint).json(&body);
        if !self.api_key.is_empty() {
            // The API keys on a header of its own; a bearer token is rejected.
            req = req.header("x-goog-api-key", &self.api_key);
        }
        let resp = req.send().await.map_err(|e| {
            Mem0Error::llm(format!(
                "SystemOne decision request failed for POST {endpoint}: {e}"
            ))
        })?;
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Mem0Error::llm(format!(
                "SystemOne decision HTTP {status} for POST {endpoint}: {}",
                provider_message(&raw)
            )));
        }
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
            Mem0Error::llm(format!(
                "SystemOne decision returned non-JSON ({e}): {}",
                truncate(&raw, 400)
            ))
        })?;
        let answers = parsed.get("answers").ok_or_else(|| {
            Mem0Error::llm(format!(
                "SystemOne decision response has no \"answers\" object: {}",
                truncate(&raw, 400)
            ))
        })?;
        Ok((self.pick(answers), answers.clone()))
    }

    /// Classify without the raw answers.
    pub async fn classify(&self, text: &str) -> Result<Vec<String>> {
        self.classify_detailed(text).await.map(|(cats, _)| cats)
    }

    /// Map an `answers` object to preset categories, preserving the preset order.
    fn pick(&self, answers: &Value) -> Vec<String> {
        let mut out = Vec::new();
        for name in PRESET_CATEGORIES {
            let Some(answer) = answers.get(*name) else {
                continue;
            };
            let Some(noul) = answer.get("noul").and_then(|v| v.as_f64()) else {
                continue;
            };
            if !(0.0..=1.0).contains(&noul) {
                continue;
            }
            // `noul` is P(the condition holds), so a category applies at or above the
            // threshold. Inverting this still stores, exports and looks like an answer.
            if noul >= self.noul_threshold {
                out.push((*name).to_string());
            }
        }
        // `misc` asks "does this fit none of the other categories", so it cannot be true
        // alongside one. Real answers do it anyway — 0.53 for `misc` next to 0.98 for
        // `family` — and a `misc` bucket on every memory is worse than an empty one.
        if out.len() > 1 {
            out.retain(|c| c != "misc");
        }
        out
    }
}

/// The provider's own explanation, when the body carries one.
///
/// The API wraps it as `{"error": {"message": "..."}}`; a truncated blob of JSON in
/// `last_error` tells an operator nothing.
fn provider_message(raw: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if let Some(msg) = parsed
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return truncate(msg, 400);
        }
    }
    truncate(raw, 400)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(cfg: Value) -> DecisionClient {
        DecisionClient::from_config(&cfg).expect("client")
    }

    #[test]
    fn defaults_are_googles_endpoint_and_model() {
        let c = client(json!({}));
        assert_eq!(c.base_url(), DEFAULT_BASE_URL);
        assert_eq!(
            c.endpoint(),
            "https://generativelanguage.googleapis.com/v1beta/decisions"
        );
        assert_eq!(c.model(), "gemini-decision-1");
        assert_eq!(c.noul_threshold, 0.5);
    }

    #[test]
    fn a_pasted_endpoint_is_not_appended_twice() {
        for (given, expected) in [
            (
                "https://generativelanguage.googleapis.com/v1beta/decisions",
                "https://generativelanguage.googleapis.com/v1beta/decisions",
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta",
                "https://generativelanguage.googleapis.com/v1beta/decisions",
            ),
            (
                "https://proxy.example/",
                "https://proxy.example/v1beta/decisions",
            ),
        ] {
            let c = client(json!({ "base_url": given }));
            assert_eq!(c.endpoint(), expected, "{given}");
        }
    }

    /// The removed vendor is refused instead of being reinterpreted: its endpoint and key
    /// belong to a service this backend no longer talks to.
    #[test]
    fn the_removed_vendor_is_rejected_with_an_explanation() {
        let err = client_err(json!({"vendor": "startlux"}));
        assert!(err.contains("no longer supported"), "{err}");
        assert!(err.contains("vendor"), "{err}");
        // Absent, empty, and "gemini" are all fine.
        assert_eq!(
            client(json!({"vendor": "gemini"})).model(),
            "gemini-decision-1"
        );
        assert_eq!(client(json!({"vendor": ""})).model(), "gemini-decision-1");
    }

    #[test]
    fn out_of_range_threshold_is_rejected() {
        let err = client_err(json!({"noul_threshold": 1.5}));
        assert!(err.contains("noul_threshold"), "{err}");
    }

    #[test]
    fn a_custom_threshold_is_read_in_the_same_direction() {
        let answers = json!({"food": {"type": "noul", "noul": 0.6}});
        assert_eq!(client(json!({})).pick(&answers), vec!["food".to_string()]);
        assert!(client(json!({"noul_threshold": 0.7}))
            .pick(&answers)
            .is_empty());
        assert_eq!(
            client(json!({"noul_threshold": 0.6})).pick(&answers),
            vec!["food".to_string()],
            "the threshold is inclusive"
        );
    }

    /// Both numbers are real responses from the live endpoint: a cracked mug scores 0.98 on
    /// "did it arrive damaged?", an intact one 0.022 on the same question.
    #[test]
    fn a_high_noul_means_the_category_applies() {
        let c = client(json!({}));
        let damaged = json!({"health": {"type": "noul", "noul": 0.9797708352463237}});
        assert_eq!(c.pick(&damaged), vec!["health".to_string()]);

        let intact = json!({"health": {"type": "noul", "noul": 0.022462306588104435}});
        assert!(c.pick(&intact).is_empty(), "{:?}", c.pick(&intact));
    }

    /// "I love ramen and hiking on weekends" scored technology 0.019 and food 0.886.
    #[test]
    fn a_real_answer_set_picks_the_right_category() {
        let answers = json!({
            "technology": {"type": "noul", "noul": 0.01899334304020548, "confidence": 0.98},
            "food": {"type": "noul", "noul": 0.8856127281366428, "confidence": 0.89},
        });
        assert_eq!(client(json!({})).pick(&answers), vec!["food".to_string()]);
    }

    /// `misc` means "none of the others", so it cannot be true alongside another category.
    #[test]
    fn misc_is_a_fallback_and_yields_to_any_other_category() {
        let c = client(json!({}));
        let with_another = json!({
            "family": {"type": "noul", "noul": 0.98},
            "misc": {"type": "noul", "noul": 0.53},
        });
        assert_eq!(c.pick(&with_another), vec!["family".to_string()]);

        let alone = json!({"misc": {"type": "noul", "noul": 0.71}});
        assert_eq!(c.pick(&alone), vec!["misc".to_string()]);
    }

    #[test]
    fn missing_non_numeric_and_out_of_range_answers_are_skipped() {
        let answers = json!({
            "food": {"type": "noul"},
            "travel": {"type": "noul", "noul": "0.9"},
            "sports": {"type": "noul", "noul": 1.4},
            "music": {"type": "noul", "noul": 0.9},
            "health": {"type": "choice", "choice": "health", "none_probability": 0.05},
            "bogus_category": {"type": "noul", "noul": 0.9},
        });
        // `none_probability` belonged to the removed vendor and is not read: an absent `noul`
        // means "no answer", not "0".
        assert_eq!(client(json!({})).pick(&answers), vec!["music".to_string()]);
    }

    #[test]
    fn the_body_carries_input_and_no_unknown_fields() {
        let body = client(json!({})).request_body("I love ramen");
        assert_eq!(body["input"], json!("I love ramen"));
        assert_eq!(body["model"], json!("gemini-decision-1"));
        assert_eq!(
            body["questions"].as_object().map(|q| q.len()),
            Some(PRESET_CATEGORIES.len())
        );
        assert_eq!(body["questions"]["food"]["type"], json!("noul"));
        // Anything else is a 400: the API rejects fields it does not know.
        assert_eq!(
            body.as_object().map(|b| b.len()),
            Some(3),
            "only model/input/questions: {body}"
        );
    }

    #[test]
    fn googles_error_message_is_extracted() {
        assert_eq!(
            provider_message(
                r#"{"error":{"code":404,"message":"Requested model not found.","status":"NOT_FOUND"}}"#
            ),
            "Requested model not found."
        );
        // A body without the envelope is passed through rather than swallowed.
        assert_eq!(provider_message("upstream exploded"), "upstream exploded");
    }

    fn client_err(cfg: Value) -> String {
        DecisionClient::from_config(&cfg)
            .err()
            .expect("must reject")
            .to_string()
    }
}
