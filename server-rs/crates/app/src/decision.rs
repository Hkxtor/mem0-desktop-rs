//! The SystemOne **decision** protocol — a second classification backend.
//!
//! Most `systemone` providers are OpenAI-compatible chat models: one prompt, one JSON
//! array back (see `crate::categories`). The SystemOne decision API is not chat: it takes
//! a state plus a map of typed questions and returns one answer per question:
//!
//! ```text
//! POST {base_url}
//! {"model": "...", "state": "<text>", "think_mode": "auto",
//!  "questions": {"food": {"type": "noul", "instructions": "..."}}}
//! -> {"answers": {"food": {"type": "noul", "noul": 0.02}}, "usage": {...}}
//! ```
//!
//! Multi-label falls out of asking one `noul` question per preset category: `noul` is the
//! probability that the category does **not** apply, so a category is included when that
//! probability is below `noul_threshold` (default 0.5). One request, fifteen answers,
//! several categories — no per-category round trip.
//!
//! This module never guesses: an answer that is missing, not a number, or outside 0..1 is
//! treated as "does not apply" rather than being coerced, and a response without an
//! `answers` object is an error carrying the provider's own body.

use mem0::{Mem0Error, Result};
use serde_json::{json, Value};

use crate::categories::PRESET_CATEGORIES;

/// Placeholder endpoint for the SystemOne decision API. There is deliberately no default:
/// this provider speaks a vendor-specific protocol, so pointing at anything by default
/// would silently send memory text to a service the operator never chose.
pub const DEFAULT_BASE_URL: &str = "";

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

/// A SystemOne decision client. Built once per configuration, like the chat classifier.
pub struct DecisionClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    think_mode: String,
    noul_threshold: f64,
}

impl DecisionClient {
    /// Build from the `systemone.config` block. `base_url` wins over the provider-specific
    /// key so an operator can point at a self-hosted instance without renaming anything.
    pub fn from_config(cfg: &Value) -> Result<Self> {
        let base_url = cfg
            .get("base_url")
            .or_else(|| cfg.get("systemone_base_url"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Mem0Error::configuration(
                    "systemone.config.base_url is required: this provider is a vendor-specific \
                     decision API, so there is no default endpoint to fall back to",
                )
            })?
            .trim_end_matches('/')
            .to_string();
        let model = cfg
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("startlux-decision-27b-autothink")
            .to_string();
        let api_key = cfg
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let think_mode = cfg
            .get("think_mode")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("auto")
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
            think_mode,
            noul_threshold,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
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
        json!({
            "model": self.model,
            "state": text,
            "questions": Value::Object(questions),
            "think_mode": self.think_mode,
        })
    }

    /// Classify, returning both the categories and the provider's raw answer map. The raw
    /// map is what `/systemone/test` shows, so a mis-tuned threshold is diagnosable
    /// without reading server logs.
    pub async fn classify_detailed(&self, text: &str) -> Result<(Vec<String>, Value)> {
        let body = self.request_body(text);
        let mut req = self.http.post(&self.base_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.map_err(|e| {
            Mem0Error::llm(format!(
                "SystemOne decision request failed for POST {}: {e}",
                self.base_url
            ))
        })?;
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Mem0Error::llm(format!(
                "SystemOne decision HTTP {status} for POST {}: {}",
                self.base_url,
                truncate(&raw, 400)
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
            // `noul` is P(category does not apply). Accept `none_probability` as well:
            // the API uses that name for the same number on `choice` answers.
            let not_applies = answer
                .get("noul")
                .or_else(|| answer.get("none_probability"))
                .and_then(|v| v.as_f64());
            let Some(not_applies) = not_applies else {
                continue;
            };
            if !(0.0..=1.0).contains(&not_applies) {
                continue;
            }
            if not_applies < self.noul_threshold {
                out.push((*name).to_string());
            }
        }
        out
    }
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

    /// Every other test names an endpoint explicitly: there is no baked-in default, because
    /// this provider would otherwise send memory text to a service nobody chose.
    fn client(cfg: Value) -> DecisionClient {
        let mut cfg = cfg;
        if !cfg.get("base_url").is_some() {
            cfg["base_url"] = json!("https://decision.test/v1/systemone");
        }
        DecisionClient::from_config(&cfg).expect("client")
    }

    #[test]
    fn defaults_when_config_is_empty_apart_from_the_endpoint() {
        let c = client(json!({}));
        assert_eq!(c.base_url(), "https://decision.test/v1/systemone");
        assert_eq!(c.model, "startlux-decision-27b-autothink");
        assert_eq!(c.think_mode, "auto");
        assert_eq!(c.noul_threshold, 0.5);
    }

    #[test]
    fn a_missing_endpoint_is_an_error_not_a_silent_default() {
        let err = DecisionClient::from_config(&json!({}))
            .err()
            .expect("must reject");
        assert!(err.to_string().contains("base_url"), "{err}");
    }

    #[test]
    fn base_url_trailing_slash_and_threshold_are_handled() {
        let c = client(json!({
            "base_url": "https://example.test/v1/systemone/",
            "noul_threshold": 0.2,
            "model": "m",
            "think_mode": "off",
        }));
        assert_eq!(c.base_url(), "https://example.test/v1/systemone");
        assert_eq!(c.noul_threshold, 0.2);
        assert_eq!(c.model, "m");
        assert_eq!(c.think_mode, "off");
    }

    #[test]
    fn out_of_range_threshold_is_rejected() {
        let err = DecisionClient::from_config(&json!({
            "base_url": "https://decision.test/v1/systemone",
            "noul_threshold": 1.5,
        }))
        .err()
        .expect("must reject");
        assert!(err.to_string().contains("noul_threshold"), "{err}");
    }

    #[test]
    fn request_body_asks_one_noul_question_per_category() {
        let body = client(json!({})).request_body("hello");
        let questions = body["questions"].as_object().expect("questions");
        assert_eq!(questions.len(), PRESET_CATEGORIES.len());
        for name in PRESET_CATEGORIES {
            assert_eq!(questions[*name]["type"], json!("noul"), "{name}");
            assert!(
                questions[*name]["instructions"]
                    .as_str()
                    .unwrap_or_default()
                    .len()
                    > 20,
                "{name}"
            );
        }
        assert_eq!(body["state"], json!("hello"));
        assert_eq!(body["think_mode"], json!("auto"));
    }

    #[test]
    fn pick_includes_categories_below_the_threshold_only() {
        let c = client(json!({}));
        let answers = json!({
            "technology": {"type": "noul", "noul": 0.02},
            "food": {"type": "noul", "noul": 0.4},
            "travel": {"type": "noul", "noul": 0.6},
            "bogus_category": {"type": "noul", "noul": 0.0},
            "health": {"type": "noul", "noul": "high"},
            "music": {"type": "choice", "none_probability": 0.1},
        });
        // preset order, unknown key and non-numeric answer dropped
        assert_eq!(
            c.pick(&answers),
            vec![
                "food".to_string(),
                "music".to_string(),
                "technology".to_string()
            ]
        );
    }

    #[test]
    fn pick_respects_a_custom_threshold() {
        let c = client(json!({"noul_threshold": 0.05}));
        let answers = json!({"technology": {"noul": 0.02}, "food": {"noul": 0.4}});
        assert_eq!(c.pick(&answers), vec!["technology".to_string()]);
    }
}
