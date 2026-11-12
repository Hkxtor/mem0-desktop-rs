//! The SystemOne **decision** protocol — a second classification backend, in two variants.
//!
//! Most `systemone` providers are OpenAI-compatible chat models: one prompt, one JSON
//! array back (see `crate::categories`). A decision API is not chat: it takes one input
//! plus a map of typed questions and returns one answer per question. Multi-label falls out
//! of asking one `noul` question per preset category — one request, fifteen answers,
//! several categories, no per-category round trip.
//!
//! Two vendors speak this shape with three differences, all confined to
//! [`DecisionVendor`]:
//!
//! ```text
//! StartLux (vendor absent)                       Gemini (vendor: "gemini")
//! POST {base_url}                                POST {base_url}/v1beta/decisions
//! Authorization: Bearer <key>                    x-goog-api-key: <key>
//! {"model", "state", "think_mode", "questions"}  {"model", "input", "questions"}
//! noul = P(category does NOT apply)              noul = P(condition holds)
//! ```
//!
//! **The last row is the trap.** Reusing one comparison for both vendors inverts every
//! classification into nonsense that still looks like a plausible answer, so the polarity
//! lives in [`DecisionVendor::applies`] and both directions are pinned by unit tests whose
//! numbers come from real responses. Google's documentation is explicit for the Gemini
//! side ("`noul`: a yes/no check that returns the probability the condition holds"); the
//! StartLux side was measured against its own wire.
//!
//! This module never guesses: an answer that is missing, not a number, or outside 0..1 is
//! skipped rather than coerced, and a response without an `answers` object is an error
//! carrying the vendor's own message.

use mem0::{Mem0Error, Result};
use serde_json::{json, Value};

use crate::categories::PRESET_CATEGORIES;

/// The StartLux decision API has no default endpoint. It speaks a vendor-specific protocol,
/// so pointing at anything by default would silently send memory text to a service the
/// operator never chose. Gemini is different: the operator names Google as the vendor, so
/// there is nothing to guess.
pub const DEFAULT_BASE_URL: &str = "";
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const GEMINI_DEFAULT_MODEL: &str = "gemini-decision-1";

/// Which decision API a `systemone` decision client talks to.
///
/// This is *not* the same axis as `systemone.provider`: `provider: "gemini"` means "use the
/// Gemini chat model as the classifier" and goes through the prompt path in
/// `crate::categories`. A vendor here only exists underneath `provider: "systemone"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionVendor {
    /// The default wire: a vendor decision API that reports P(category does not apply).
    StartLux,
    /// `generativelanguage.googleapis.com/v1beta/decisions`, model `gemini-decision-1`.
    Gemini,
}

impl DecisionVendor {
    fn parse(cfg: &Value) -> Result<Self> {
        match cfg.get("vendor").and_then(|v| v.as_str()).map(str::trim) {
            None | Some("") | Some("startlux") => Ok(Self::StartLux),
            Some("gemini") => Ok(Self::Gemini),
            Some(other) => Err(Mem0Error::configuration(format!(
                "systemone vendor {other:?} is not supported; use \"gemini\" or leave it out"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::StartLux => "startlux",
            Self::Gemini => "gemini-decision",
        }
    }

    /// Does an answer mean "this category applies"?
    ///
    /// One knob, opposite comparisons — see the module docs for why this cannot be shared.
    fn applies(self, noul: f64, threshold: f64) -> bool {
        match self {
            // Google: "`noul`: a yes/no check that returns the probability the condition
            // holds". Measured: a cracked-mug message scores 0.98 on "did it arrive
            // damaged?", an intact one 0.022 on the same question.
            Self::Gemini => noul >= threshold,
            // StartLux reports the complement: a ramen message scored 0.02 on "does this
            // concern technology" and 0.886 on "does this concern food".
            Self::StartLux => noul < threshold,
        }
    }

    /// Which field carries the text being classified.
    fn input_field(self) -> &'static str {
        match self {
            Self::StartLux => "state",
            Self::Gemini => "input",
        }
    }
}

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
    vendor: DecisionVendor,
}

impl DecisionClient {
    /// Build from the `systemone.config` block. `base_url` wins over the provider-specific
    /// key so an operator can point at a self-hosted instance without renaming anything.
    pub fn from_config(cfg: &Value) -> Result<Self> {
        let vendor = DecisionVendor::parse(cfg)?;
        let configured_base = cfg
            .get("base_url")
            .or_else(|| cfg.get("systemone_base_url"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let base_url =
            match (configured_base, vendor) {
                (Some(url), _) => url.trim_end_matches('/').to_string(),
                // Naming Google as the vendor is the choice; defaulting the endpoint is not a
                // guess about who receives the text.
                (None, DecisionVendor::Gemini) => GEMINI_BASE_URL.to_string(),
                (None, DecisionVendor::StartLux) => return Err(Mem0Error::configuration(
                    "systemone.config.base_url is required: this provider is a vendor-specific \
                     decision API, so there is no default endpoint to fall back to",
                )),
            };
        let default_model = match vendor {
            DecisionVendor::StartLux => "startlux-decision-27b-autothink",
            DecisionVendor::Gemini => GEMINI_DEFAULT_MODEL,
        };
        let model = cfg
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_model)
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
            .map_err(|e| {
                Mem0Error::configuration(format!("systemone ({}): {e}", vendor.label()))
            })?;
        Ok(Self {
            http,
            base_url,
            api_key,
            model,
            think_mode,
            noul_threshold,
            vendor,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn vendor(&self) -> DecisionVendor {
        self.vendor
    }

    /// The model in effect, including the vendor default when the config left it blank.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Where the request actually goes. Gemini keeps its RPC under a fixed path; accepting a
    /// base URL that already ends there keeps "paste the full endpoint" from producing
    /// `/v1beta/decisions/v1beta/decisions`.
    fn endpoint(&self) -> String {
        match self.vendor {
            DecisionVendor::StartLux => self.base_url.clone(),
            DecisionVendor::Gemini if self.base_url.ends_with("/decisions") => {
                self.base_url.clone()
            }
            DecisionVendor::Gemini if self.base_url.ends_with("/v1beta") => {
                format!("{}/decisions", self.base_url)
            }
            DecisionVendor::Gemini => format!("{}/v1beta/decisions", self.base_url),
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
        let mut body = json!({
            "model": self.model,
            self.vendor.input_field(): text,
            "questions": Value::Object(questions),
        });
        // `think_mode` is a StartLux parameter. Gemini rejects unknown fields, so sending it
        // there would turn every classification into a 400.
        if self.vendor == DecisionVendor::StartLux {
            body["think_mode"] = json!(self.think_mode);
        }
        body
    }

    /// Classify, returning both the categories and the provider's raw answer map. The raw
    /// map is what `/systemone/test` shows, so a mis-tuned threshold is diagnosable
    /// without reading server logs.
    pub async fn classify_detailed(&self, text: &str) -> Result<(Vec<String>, Value)> {
        let body = self.request_body(text);
        let endpoint = self.endpoint();
        let mut req = self.http.post(&endpoint).json(&body);
        if !self.api_key.is_empty() {
            req = match self.vendor {
                // The Gemini API keys on a header of its own; a bearer token is rejected.
                DecisionVendor::Gemini => req.header("x-goog-api-key", &self.api_key),
                DecisionVendor::StartLux => req.bearer_auth(&self.api_key),
            };
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
            // StartLux also uses `none_probability` for the same number on `choice` answers.
            let noul = answer
                .get("noul")
                .or_else(|| match self.vendor {
                    DecisionVendor::StartLux => answer.get("none_probability"),
                    DecisionVendor::Gemini => None,
                })
                .and_then(|v| v.as_f64());
            let Some(noul) = noul else {
                continue;
            };
            if !(0.0..=1.0).contains(&noul) {
                continue;
            }
            if self.vendor.applies(noul, self.noul_threshold) {
                out.push((*name).to_string());
            }
        }
        // `misc` asks "does this fit none of the other categories", so selecting it next to
        // another category contradicts itself. Real answers do it anyway — the model scores
        // it 0.53 "maybe" on a family memory that it scores 0.98 for `family` — and a `misc`
        // bucket that fills up on every memory is worse than an empty one.
        if out.len() > 1 {
            out.retain(|c| c != "misc");
        }
        out
    }
}

/// The vendor's own explanation, when the body carries one.
///
/// Gemini wraps it as `{"error": {"message": "..."}}` and StartLux answers with a bare text
/// body; a truncated blob of JSON in `last_error` tells an operator nothing.
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

    /// Every other test names an endpoint explicitly: there is no baked-in default, because
    /// this provider would otherwise send memory text to a service nobody chose.
    fn client(cfg: Value) -> DecisionClient {
        let mut cfg = cfg;
        if cfg.get("base_url").is_none() {
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

    // ---- Gemini vendor ----

    fn gemini(cfg: Value) -> DecisionClient {
        let mut cfg = cfg;
        cfg["vendor"] = json!("gemini");
        DecisionClient::from_config(&cfg).expect("client")
    }

    #[test]
    fn gemini_defaults_to_googles_endpoint_and_model() {
        // Unlike StartLux, naming the vendor *is* the choice of service, so an endpoint
        // default here is not a silent redirect.
        let c = gemini(json!({}));
        assert_eq!(c.base_url(), "https://generativelanguage.googleapis.com");
        assert_eq!(
            c.endpoint(),
            "https://generativelanguage.googleapis.com/v1beta/decisions"
        );
        assert_eq!(c.model, "gemini-decision-1");
        assert_eq!(c.vendor(), DecisionVendor::Gemini);
    }

    #[test]
    fn a_pasted_gemini_endpoint_is_not_appended_twice() {
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
            let c = gemini(json!({ "base_url": given }));
            assert_eq!(c.endpoint(), expected, "{given}");
        }
    }

    #[test]
    fn an_unknown_vendor_is_rejected_rather_than_defaulted() {
        let err = DecisionClient::from_config(&json!({"vendor": "openai-decisions"}))
            .err()
            .expect("must reject");
        assert!(err.to_string().contains("vendor"), "{err}");
    }

    /// The bug this whole per-vendor split exists for. Both vectors are real responses from
    /// the live endpoint: a cracked mug scores 0.98 on "did it arrive damaged?" and an intact
    /// one 0.022 on the same question.
    #[test]
    fn gemini_treats_a_high_noul_as_applies() {
        let c = gemini(json!({}));
        let damaged = json!({"health": {"type": "noul", "noul": 0.9797708352463237}});
        assert_eq!(c.pick(&damaged), vec!["health".to_string()]);

        let intact = json!({"health": {"type": "noul", "noul": 0.022462306588104435}});
        assert!(c.pick(&intact).is_empty(), "{:?}", c.pick(&intact));
    }

    /// The same numbers through the StartLux comparison select the opposite answer — which is
    /// exactly what a shared `pick` would have done to production traffic.
    #[test]
    fn the_two_vendors_read_the_same_number_oppositely() {
        let answers = json!({"health": {"type": "noul", "noul": 0.98}});
        assert_eq!(gemini(json!({})).pick(&answers), vec!["health".to_string()]);
        assert!(
            client(json!({})).pick(&answers).is_empty(),
            "StartLux reads noul as P(does not apply)"
        );
    }

    #[test]
    fn gemini_reads_the_threshold_the_same_way_round() {
        let answers = json!({"food": {"type": "noul", "noul": 0.6}});
        assert!(gemini(json!({"noul_threshold": 0.7}))
            .pick(&answers)
            .is_empty());
        assert_eq!(
            gemini(json!({"noul_threshold": 0.5})).pick(&answers),
            vec!["food".to_string()]
        );
    }

    /// A real Gemini answer set: "I love ramen and hiking on weekends" scored technology
    /// 0.019 and food 0.886.
    #[test]
    fn gemini_picks_from_a_real_answer_set() {
        let c = gemini(json!({}));
        let answers = json!({
            "technology": {"type": "noul", "noul": 0.01899334304020548, "confidence": 0.98},
            "food": {"type": "noul", "noul": 0.8856127281366428, "confidence": 0.89},
        });
        assert_eq!(c.pick(&answers), vec!["food".to_string()]);
    }

    /// `misc` means "none of the others", so it cannot be true alongside another category.
    #[test]
    fn misc_is_a_fallback_and_yields_to_any_other_category() {
        let c = gemini(json!({}));
        let with_another = json!({
            "family": {"type": "noul", "noul": 0.98},
            "misc": {"type": "noul", "noul": 0.53},
        });
        assert_eq!(c.pick(&with_another), vec!["family".to_string()]);

        // Alone, it is the only thing that applies and must survive.
        let alone = json!({"misc": {"type": "noul", "noul": 0.71}});
        assert_eq!(c.pick(&alone), vec!["misc".to_string()]);
    }

    #[test]
    fn gemini_skips_missing_non_numeric_and_out_of_range_answers() {
        let c = gemini(json!({}));
        let answers = json!({
            "food": {"type": "noul"},
            "travel": {"type": "noul", "noul": "0.9"},
            "sports": {"type": "noul", "noul": 1.4},
            "music": {"type": "noul", "noul": 0.9},
            "health": {"type": "choice", "choice": "health", "none_probability": 0.05},
        });
        // `none_probability` is a StartLux convention and is not read for Gemini: an absent
        // `noul` means "no answer", not "0".
        assert_eq!(c.pick(&answers), vec!["music".to_string()]);
    }

    #[test]
    fn gemini_sends_input_without_think_mode() {
        let body = gemini(json!({})).request_body("I love ramen");
        assert_eq!(body["input"], json!("I love ramen"));
        assert_eq!(body["model"], json!("gemini-decision-1"));
        assert!(
            body.get("state").is_none(),
            "`state` is the StartLux field name: {body}"
        );
        assert!(
            body.get("think_mode").is_none(),
            "Gemini rejects unknown fields: {body}"
        );
        assert_eq!(
            body["questions"].as_object().map(|q| q.len()),
            Some(PRESET_CATEGORIES.len())
        );
    }

    /// The StartLux wire is the existing contract and must not drift while Gemini is added.
    #[test]
    fn the_startlux_wire_is_unchanged() {
        let body = client(json!({})).request_body("hello");
        assert_eq!(body["state"], json!("hello"));
        assert_eq!(body["think_mode"], json!("auto"));
        assert!(body.get("input").is_none(), "{body}");
    }

    #[test]
    fn a_vendor_message_is_extracted_from_googles_error_envelope() {
        assert_eq!(
            provider_message(
                r#"{"error":{"code":404,"message":"Requested model not found.","status":"NOT_FOUND"}}"#
            ),
            "Requested model not found."
        );
        // StartLux answers with a bare body; nothing to unwrap.
        assert_eq!(provider_message("upstream exploded"), "upstream exploded");
    }
}
