# CONTEXT — mem0-desktop Rust server (`server-rs/`)

Glossary only. No implementation details, no decisions (those go to ADRs if they earn one).

## Memory

- **Memory（记忆）** — one extracted fact stored in `vectors.db` (sqlite-vec). Carries a payload: `data`, `hash`, `created_at`, `updated_at`, `expiration_date`, scope keys (`user_id`/`agent_id`/`run_id`), and `categories`.
- **Extraction（提取）** — the step inside `add` that turns submitted messages into candidate memories. A result count of zero means extraction produced nothing: legitimate for chatter, indistinguishable from a broken model answer unless the answer itself is visible.
- **Entity（实体）** — a bucket derived from a memory's scope keys: `user` from `user_id`, `agent` from `agent_id`, `run` from `run_id`. Computed on read from stored memories, never stored on its own, so an entity exists exactly as long as at least one visible memory carries its id. An empty Entities page therefore means "no memories stored with a scope key", not "the index is stale".
- **Category（分类）** — a label from the fixed preset of 15 (see SystemOne). A memory carries 0..n categories; multi-label is the norm (official exports contain multi-label rows). Stored as a JSON array in the memory payload under `categories`; filterable with AND semantics (a memory matches only if it contains every requested category).
- **SystemOne** — the dedicated classification model configuration section, parallel to `llm` and `embedder`. Classification only runs when this section is explicitly configured; otherwise memories are stored with `categories: []` and no error. Exposed over HTTP as `POST /systemone/classify` (text in → category list out) and used internally by the add pipeline.
- **Classification（分类）** — mapping a memory's text to a subset of the 15 preset categories via the SystemOne model. Best-effort: if the call fails, the memory is still stored with empty categories; the failure is recorded as `last_error` and surfaced in the UI rather than only logged.
- **Unclassified（未分类）** — a memory whose `categories` array is empty. The term deliberately does not distinguish "never classified" from "classified, and the model found no matching category": no attempt is recorded. It means "carries no category right now", so any unclassified memory is always eligible for another classification run.

## Access

- **Key Pool（密钥池）** — the set of API keys a single model section (`llm`, `embedder`, `systemone`) may spend. One pool per section, all keys interchangeable: same provider, same model, so a request's outcome does not depend on which key served it. A single-key configuration is a pool of one and behaves exactly as it did before pools existed.
- **Cooldown（冷却）** — the state of a pooled key that has just answered `429`. A cooling key is skipped rather than retried, and returns to the pool on its own when the period ends; nothing about the pool is permanent, so a restart forgets every cooldown. Distinct from a *revoked* key, which no amount of waiting fixes.
- **Revoked key（作废密钥）** — a pooled key whose credential the upstream rejected (`401`/`403`). It is not cooling: waiting will not help, so it stays out of the pool until the process restarts or the keys are edited. Distinct from a *rate-limited* key, which is only temporarily out. The pool is not broken while one key is revoked, so a request served by another key succeeds — but the revocation must be visible, since it means that key is wrong, cancelled, or unpaid.

- **Access token（访问令牌）** — short-lived bearer token (1 h) minted by `/auth/login`. The dashboard keeps it in memory only, so it disappears on reload by design and is replaced from the refresh cookie.
- **Refresh token（刷新令牌）** — 30-day credential exchanged for a new access token. Lives in an `HttpOnly` cookie the page cannot read, so a script injected into the dashboard cannot exfiltrate a month of access. Single use: each exchange revokes the token it consumed and returns a replacement, which the cookie must be updated to or the *next* refresh fails.
- **Session（会话）** — the browser's ability to obtain an access token without a password. Held entirely by the refresh cookie; the server stores no session object. Ends when the cookie is cleared (`DELETE /auth/session`) or its token is exchanged and the replacement is lost.
- **Public path（公开路径）** — a route that answers without a credential: the dashboard's own files and the sign-in endpoints (`/auth/setup-status`, `/auth/register`, `/auth/login`, `/auth/refresh`, `/auth/session`). Everything that touches a user's data is not. A path missing from this set is not a subtle bug — with authentication enabled the login page itself would answer `401`.

## Records

Three separate records answer three different questions. None of them is a superset of another.

- **Memory Operation（记忆操作）** — one memory-level action (ADD / SEARCH / DELETE / …): what was asked, how many results, status, latency. Answers "what did the app do to my memories". Backs the Requests page.
- **Request Log（请求日志）** — one HTTP request: method, path, status, latency, credential type. Answers "what traffic arrived". Backs the Analytics figures and the request-log tab. Deliberately excludes memory operations and vice versa: looking at statistics must not change the statistics.
- **Application Log（应用日志）** — the operational narrative of the server and desktop shell: startup, configuration, external calls, storage, auth rejections, shell lifecycle. Answers "why did it behave this way". Lives in a rotating file inside the data directory, not in the database, so it survives a failure that happens before the database opens.
