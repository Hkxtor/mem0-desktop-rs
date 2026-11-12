# API keys are pooled per model section

**Status**: accepted

One API key is one quota, and a single quota is a single point of failure: the upstream answers
`429` under load, and `add`, `import` and batch classification each fan one user action out into
many calls. So each model section (`llm`, `embedder`, `systemone`) may hold a **pool** of keys —
`config.api_keys`, with the legacy `api_key` kept as "the first one" so every existing config
file and contract keeps its meaning. A pool of size one is the only thing that existed before,
and it behaves identically.

Requests round-robin across the usable keys. A key that answers `429` enters a **cooldown**
(honouring `Retry-After`, otherwise 60 s) and is skipped until it expires; the request that hit
it re-issues against the next usable key immediately. A key that answers `401`/`403` is
**revoked** for the lifetime of the process rather than cooled — waiting cannot repair a
cancelled credential. `5xx` and transport failures do not rotate: the same upstream is broken
for every key, and rotating would turn one failure into three slow ones. When every key is out,
the request fails at once with the earliest recovery time in the message — unless that is under
3 seconds, in which case it waits, because failing one second early is the worst of both.

## Considered options

- **Failover only** (spend key one until it dies, then key two). Rejected: the first key's quota
  is exactly what runs out first, so a pool would mostly buy "the pool works, eventually".
- **Round-robin without cooldown.** Rejected: the rotation re-aims requests at the key that just
  refused them, which is one failure rewritten as several.
- **Wait out the cooldown inside the request.** Rejected: this is an interactive desktop app, and
  an HTTP request hanging for a minute reads as a hang, not as patience.
- **Heterogeneous pool** (each key carrying its own provider / model / endpoint). Rejected: a
  request's outcome must not depend on which key served it. Mixed embedders mean inconsistent
  vector dimensions and a dead index; mixed LLMs mean the same memory classified differently
  depending on rotation order.

## Consequences

- **Which key paid is nondeterministic.** Per-key accounting lives in the application log line,
  not in the config order.
- **Pool state is in-memory only**, so a restart re-arms every revoked key. Deliberate: the usual
  reason a key was revoked is that it was wrong or unpaid, and both are fixed outside this
  process. Persisting it would mean a fixed key could never come back without a file edit.
- **`embedder` and `systemone` inherit the `llm` pool** when they name no keys and share its
  provider, so those sections are no longer self-contained: an empty key field there means "the
  LLM's pool", not "no credential". What they inherit is the **key list, not the cooldown state**
  — quotas are per model, so a key parked by an embedding rate limit must not also be parked for
  chat. The alternative — the dashboard copying the LLM keys into each section on save — was
  rejected because the copies go stale the moment the LLM keys change, and the embedder is the
  section that hits quota first.
- **A `429` no longer fails the whole request**, so failure counts that gate longer operations
  (batch classification stops after three consecutive failures) count only unrecoverable
  failures. A rotation that succeeds is not a failure.
