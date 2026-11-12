//! Category classification ("systemone") for the desktop app.
//!
//! The hosted platform tags every memory with one or more of a fixed preset of
//! categories (`categories` in official exports). The desktop build reproduces that
//! with a dedicated model section, `systemone`, configured independently of `llm` and
//! `embedder` in `config.json`:
//!
//! ```json
//! { "systemone": { "provider": "openai", "config": { "model": "gpt-5-mini", "api_key": "..." } } }
//! ```
//!
//! When the section is absent (or has an empty provider) classification is **off**:
//! memories are stored with `categories: []` and nothing errors. Classification is
//! best-effort everywhere: a failed classify call yields an empty list, never a failed
//! memory write.
//!
//! The preset is exactly the hosted platform's 15 categories; model output is filtered
//! against it, so unknown labels can never leak into storage.

use mem0::{GenerateOptions, Llm, Mem0Error, Message, Result};
use serde_json::{json, Value};

/// The fixed category preset — identical to the hosted platform's list.
/// Deliberately NOT extensible: import/export/filter all key off this list.
pub const PRESET_CATEGORIES: &[&str] = &[
    "personal_details",
    "family",
    "professional_details",
    "sports",
    "travel",
    "food",
    "music",
    "health",
    "technology",
    "hobbies",
    "fashion",
    "entertainment",
    "milestones",
    "user_preferences",
    "misc",
];

/// Is a `systemone` section present and enabled in the server config?
pub fn is_configured(config: &Value) -> bool {
    config
        .get("systemone")
        .and_then(|s| s.get("provider"))
        .and_then(|p| p.as_str())
        .map(|p| !p.is_empty())
        .unwrap_or(false)
}

/// Build the classifier from the server config. `Ok(None)` means classification is off.
pub fn build_classifier(config: &Value) -> Result<Option<Classifier>> {
    if !is_configured(config) {
        crate::pooled::clear(crate::pooled::SYSTEMONE);
        return Ok(None);
    }
    let section = &config["systemone"];
    let provider = section["provider"].as_str().unwrap_or_default();
    let cfg = section.get("config").cloned().unwrap_or(json!({}));
    // Classification may spend several keys of the same provider, and may spend the LLM's
    // keys when it names none — a batch run is the heaviest caller in the app.
    let (keys, inherited) = crate::pooled::resolve_keys(config, crate::pooled::SYSTEMONE, provider);
    // The SystemOne decision API is not chat-completions: it takes a state plus typed
    // questions. Everything else (openai/anthropic/gemini/mock) is a chat model.
    if provider == DECISION_PROVIDER {
        let client = crate::decision::DecisionClient::from_config(&cfg, keys, inherited)?;
        return Ok(Some(Classifier {
            backend: Backend::Decision(client),
            max_tokens: 0,
        }));
    }
    let llm: Box<dyn Llm> = if provider == "mock" {
        // The fork's MockLlm returns a fixed extraction payload, which is useless for
        // classification. The app-local canned LLM honors `config.response` so tests and
        // e2e can drive deterministic categories through the real HTTP path.
        let response = cfg
            .get("response")
            .and_then(|r| r.as_str())
            .unwrap_or("[]")
            .to_string();
        Box::new(CannedLlm(response))
    } else {
        crate::pooled::build_llm(provider, &cfg, crate::pooled::SYSTEMONE, keys, inherited)
            .map_err(|e| Mem0Error::configuration(format!("systemone ({provider}): {e}")))?
    };
    // Reasoning models (a `reasoning` field instead of `content`) spend the whole budget
    // thinking and answer with `content: null` when the budget is small. 128 tokens — the
    // obvious choice for "return a short JSON array" — silently produced zero categories
    // on a real endpoint. 4096 is enough for the model to stop thinking and answer; an
    // operator on a tighter budget can lower it.
    let max_tokens = cfg
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(8192) as u32;
    Ok(Some(Classifier {
        backend: Backend::Chat(llm),
        max_tokens,
    }))
}

/// Provider name for the SystemOne decision API (`{base_url}` receives a state plus
/// typed questions instead of a chat completion request).
pub const DECISION_PROVIDER: &str = "systemone";

/// Which protocol this classifier speaks.
enum Backend {
    Chat(Box<dyn Llm>),
    Decision(crate::decision::DecisionClient),
}

/// A classifier backed by either a chat model or the SystemOne decision API.
pub struct Classifier {
    backend: Backend,
    /// Token budget for a chat classification. Large because reasoning models count
    /// their thinking against it; the decision backend ignores it.
    max_tokens: u32,
}

impl Classifier {
    /// Classify `text` into a subset of [`PRESET_CATEGORIES`]. Never fails on model
    /// output shape — unparseable output becomes an empty list.
    pub async fn classify(&self, text: &str) -> Result<Vec<String>> {
        self.classify_detailed(text).await.map(|(cats, _)| cats)
    }

    /// Classify and, for the decision backend, also return the provider's raw answers so
    /// `/systemone/test` can show why a threshold picked what it picked.
    pub async fn classify_detailed(&self, text: &str) -> Result<(Vec<String>, Option<Value>)> {
        match &self.backend {
            Backend::Decision(client) => {
                let (cats, raw) = client.classify_detailed(text).await?;
                Ok((cats, Some(raw)))
            }
            Backend::Chat(llm) => {
                let raw = llm
                    .generate(
                        &[Message {
                            role: "user".into(),
                            content: classify_prompt(text),
                            name: None,
                        }],
                        &GenerateOptions {
                            // Deliberately NOT json_object: a real OpenAI-compatible
                            // reasoning model answered `response_format:
                            // {"type":"json_object"}` with a repeating
                            // `{"family": ["family"], "hobbies": ["hobbies"], ...` until
                            // the budget ran out, and answered the identical prompt
                            // correctly without it. The prompt already asks for a bare
                            // array and `parse_categories` tolerates fences and prose, so
                            // the flag buys nothing and breaks some providers.
                            response_format_json: false,
                            temperature: Some(0.0),
                            max_tokens: Some(self.max_tokens),
                            // Explicit 1.0, not None: the fork's OpenAI client defaults
                            // to top_p 0.1, and a real reasoning model sent top_p 0.1
                            // never stopped thinking — 4096 tokens of reasoning, then
                            // `content: null`. The identical prompt at top_p 1.0
                            // answered in 3485 reasoning tokens.
                            top_p: Some(1.0),
                            tools: None,
                        },
                    )
                    .await?;
                // An empty answer is not "no category applies" — it is the model failing
                // to answer at all, and reporting it as an empty list is exactly the
                // silent failure that made a broken setup look like a working one.
                if raw.trim().is_empty() {
                    return Err(Mem0Error::llm(format!(
                        "systemone model returned an empty answer (no content). A reasoning \
                         model that exhausts its budget answers with content: null — raise \
                         systemone.config.max_tokens (currently {})",
                        self.max_tokens
                    )));
                }
                Ok((parse_categories(&raw), None))
            }
        }
    }
}

/// The classification prompt. Asks for a bare JSON array drawn only from the preset.
pub fn classify_prompt(text: &str) -> String {
    format!(
        "Classify the following text into zero or more of these categories:\n{}\n\n\
         Rules:\n\
         - Reply with ONLY a JSON array of category names, e.g. [\"technology\", \"food\"].\n\
         - Use only category names from the list above. Never invent new ones.\n\
         - If nothing fits, reply with [].\n\n\
         Text: {}",
        PRESET_CATEGORIES
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n"),
        text
    )
}

/// Parse model output into a sanitized category list: tolerate markdown fences and
/// surrounding prose (take the first JSON array found), keep only preset names,
/// deduplicate, preserve model order.
pub fn parse_categories(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let candidate = strip_code_fence(trimmed);
    // Find the first '[' ... ']' span; if the model wrapped the array in an object
    // like {"categories": [...]}, look inside it first.
    let Some(array_text) = extract_json_array(candidate).or_else(|| extract_json_array(trimmed))
    else {
        return Vec::new();
    };
    let parsed: Value = match serde_json::from_str(&array_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(items) = parsed.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else { continue };
        if PRESET_CATEGORIES.contains(&name) && !out.iter().any(|c| c == name) {
            out.push(name.to_string());
        }
    }
    out
}

fn strip_code_fence(s: &str) -> &str {
    let t = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    t.strip_suffix("```").unwrap_or(t).trim()
}

/// Return the text of the first balanced `[...]` span in `s`, if any.
fn extract_json_array(s: &str) -> Option<String> {
    let start = s.find('[')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in s[start..].char_indices() {
        let c = ch;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=start + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Mock LLM that always returns a canned string — used for provider "mock" so tests
/// can inject an exact model response via `systemone.config.response`.
struct CannedLlm(String);

#[async_trait::async_trait]
impl Llm for CannedLlm {
    async fn generate(&self, _messages: &[Message], _options: &GenerateOptions) -> Result<String> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_array() {
        assert_eq!(
            parse_categories(r#"["technology","food"]"#),
            vec!["technology", "food"]
        );
    }

    #[test]
    fn parses_code_fenced_array() {
        assert_eq!(
            parse_categories("```json\n[\"travel\"]\n```"),
            vec!["travel"]
        );
    }

    #[test]
    fn parses_array_embedded_in_prose() {
        assert_eq!(
            parse_categories("Here you go: [\"music\", \"hobbies\"] — hope that helps"),
            vec!["music", "hobbies"]
        );
    }

    #[test]
    fn parses_object_wrapper() {
        assert_eq!(
            parse_categories(r#"{"categories": ["health", "sports"]}"#),
            vec!["health", "sports"]
        );
    }

    #[test]
    fn drops_unknown_categories_and_duplicates() {
        assert_eq!(
            parse_categories(r#"["technology","bogus","technology","MISC"]"#),
            vec!["technology"]
        );
    }

    #[test]
    fn non_json_output_is_empty() {
        assert!(parse_categories("I cannot classify this.").is_empty());
        assert!(parse_categories("").is_empty());
        assert!(parse_categories(r#"{"no": "array"}"#).is_empty());
    }

    #[test]
    fn preset_has_15_entries() {
        assert_eq!(PRESET_CATEGORIES.len(), 15);
    }

    #[test]
    fn is_configured_requires_provider() {
        assert!(!is_configured(&json!({})));
        assert!(!is_configured(&json!({"systemone": {"provider": ""}})));
        assert!(is_configured(&json!({"systemone": {"provider": "mock"}})));
    }

    #[test]
    fn build_classifier_none_without_section() {
        assert!(build_classifier(&json!({})).unwrap().is_none());
    }

    #[tokio::test]
    async fn mock_classifier_uses_canned_response() {
        let cfg = json!({"systemone": {"provider": "mock", "config": {"response": "[\"food\",\"nope\"]"}}});
        let clf = build_classifier(&cfg).unwrap().expect("configured");
        assert_eq!(clf.classify("I love ramen").await.unwrap(), vec!["food"]);
    }

    #[test]
    fn build_classifier_rejects_unknown_provider() {
        let cfg = json!({"systemone": {"provider": "no-such", "config": {}}});
        let err = match build_classifier(&cfg) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unknown provider must fail"),
        };
        assert!(err.contains("systemone"), "{err}");
        assert!(err.contains("no-such"), "{err}");
    }
}
