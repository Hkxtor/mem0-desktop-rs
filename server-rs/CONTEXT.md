# CONTEXT — mem0-desktop Rust server (`server-rs/`)

Glossary only. No implementation details, no decisions (those go to ADRs if they earn one).

- **Memory（记忆）** — one extracted fact stored in `vectors.db` (sqlite-vec). Carries a payload: `data`, `hash`, `created_at`, `updated_at`, `expiration_date`, scope keys (`user_id`/`agent_id`/`run_id`), and `categories`.
- **Category（分类）** — a label from the fixed preset of 15 (see SystemOne). A memory carries 0..n categories; multi-label is the norm (official exports contain multi-label rows). Stored as a JSON array in the memory payload under `categories`; filterable with AND semantics (a memory matches only if it contains every requested category).
- **SystemOne** — the dedicated classification model configuration section, parallel to `llm` and `embedder`. Classification only runs when this section is explicitly configured; otherwise memories are stored with `categories: []` and no error. Exposed over HTTP as `POST /systemone/classify` (text in → category list out) and used internally by the add pipeline.
- **Classification（分类）** — mapping a memory's text to a subset of the 15 preset categories via the SystemOne model. Best-effort: if the call fails, the memory is still stored with empty categories; the failure is recorded as `last_error` and surfaced in the UI rather than only logged.
- **Unclassified（未分类）** — a memory whose `categories` array is empty. The term deliberately does not distinguish "never classified" from "classified, and the model found no matching category": no attempt is recorded. It means "carries no category right now", so any unclassified memory is always eligible for another classification run.
