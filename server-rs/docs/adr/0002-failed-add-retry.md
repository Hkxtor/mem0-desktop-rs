# Failed adds are stashed so they can be retried

**Status**: accepted

An ADD that stores nothing is indistinguishable from an ADD that found nothing: both answer
`200` with `results: []`. But three of the reasons are temporary — the model call failed (`429`,
`5xx`, bad key), the answer could not be parsed into even one complete entry, or the extracted
entries could not be embedded — and by the time the operator reads the log, **the request is
gone**. The caller's messages were never written down (extraction returns before `save_messages`
on an LLM error), so there is nothing to redo. That is why a rate limit costs a memory rather
than a retry.

So a failure that *may* pass keeps its material in a new `failed_adds` table: the submitted
`messages` plus, when extraction did produce them, the entries it produced. A **retry** redoes
the work — entries that exist are re-embedded and stored, entries that do not are extracted
again — and a retry that stores something deletes the record. `no_memories` ("the model saw
nothing worth keeping") is not a failure and is not stashed: retrying it burns a call to reach
the same conclusion.

Records dedupe on a hash of `messages` + scope (`user_id`/`agent_id`/`run_id`), so a client
retrying in a loop produces one pending row with an attempt count, not fifty identical ones.
They expire after 14 days (with the existing history pruning) and are capped at 500, oldest
evicted: this table holds memory text, the most sensitive thing the app stores, and it is
exactly the table that grows when the upstream is unstable. The panel offers "delete all" for
operators who would rather not retry.

## Considered options

- **Stash nothing, retry nothing.** Status quo. Rejected: the failure families are already
  identified and explained in the log; the missing piece is only that the input was discarded.
- **Store only the messages.** Rejected: an embedding failure already has its extracted entries,
  and re-extracting to get them back spends a model call to produce *different* text — the
  memory the user finally gets would not be the one that was attempted.
- **Store only the extracted entries.** Rejected: on an LLM error there are no entries at all,
  which is the case that loses the most.
- **Reuse `request_logs` as the store.** Rejected: those rows are append-only and pruned at 14
  days, and a retry has state to hold (attempts, last error) that would turn a log into a
  little state machine.
- **Files under `data/pending/`.** Rejected: listing and counting become directory scans, and
  writing the memory plus deleting the record no longer share one transaction — a crash in
  between leaves either a memory that will be retried twice or a pending row whose memory
  already exists.
- **No dedup.** Rejected: a client retrying through an outage is the normal case, and it would
  bury the one distinct failure under its own repeats.
- **Keep resolved records for audit.** Rejected: after a successful retry the messages live on
  as that memory's Input, so keeping the row stores the same text twice — for the class of data
  that least wants a second copy.
- **No retention, or retention without a cap.** Rejected on both halves: unbounded text on a
  machine with no operator watching, or a hard cap that still keeps forgotten text forever.

## Consequences

- **The most sensitive table in the database is the one that appears during outages.** It is
  bounded (14 days, 500 rows), deletable in one action, and cleared when the add succeeds; that
  bound is the price of being able to retry at all.
- **A retry uses the current configuration**, not the configuration that failed. This is the
  point — fixing the key or switching the model is what makes old failures worth retrying —
  but it also means a retry can extract differently than the first attempt would.
- **An embedding-failure retry stores the text as-is** (`infer: false`) and writes the row's
  original messages into that memory's Input. The memory keeps its honest provenance, and the
  text is exactly what was attempted rather than whatever a fresh extraction would produce.
- **The caller is never told.** A retried add still looks, to the client, like the `200` with
  `results: []` it got at the time. Recovering it is the operator's job, on the panel — silently
  answering the original request is not possible after the connection closed.
- **`/requests` grows one nullable field** (`failed_add_id`) so the list can show "stashed" and
  jump to the record. The user-facing reason stays what it was; "already kept for you" is
  additional information, not a replacement.
- **`no_memories` is a promise that stays.** Because it is deliberately not stashed, a
  zero-result ADD with that reason can never appear in the retry drawer, however much it looks
  like a failure to the person reading the Requests page.
