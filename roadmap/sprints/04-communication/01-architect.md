---
role: architect
sprint: 04
step: 1
---

# 04/01 — Mail format, watcher & dispatch protocols

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §8, HANDOFF.md (sprint 03:
delivery queue API).

## Deliverable 1 — `hamlet-core::mail` [DEV → ADR-0009]

Mail format: plain text, minimal header (`From: <uid|name>`, `To: <uid>`,
`Subject:`, `Date:` as lineage-only data, blank line, body). Spool layout:
`courier/mail/<uid>/` — new files = incoming; naming scheme atomic
(writer-visible only when complete: temp + rename within spool). Read state
tracking (delivered/acked) so daemon restart never double-delivers — decide
where the ack marker lives (tree, per file-as-truth) and record it.

## Deliverable 2 — `hamlet-core::dispatch` [DEV → ADR-0010]

Dispatch JSON schemas: outbox file (method/url-ref, headers-ref, body,
correlation id), inbox response file (status, headers subset, body, error).
URL allowlist policy from config.json; auth material referenced from `.env`
by key name only (never inline). Retry/backoff: bounded exponential with
jitter-free deterministic schedule (state contract: reproducible), max
attempts, dead-letter subdir.

## Deliverable 3 — watcher event vocabulary (ADR-0009)

Typed FsEvent → Reaction mapping table covering: mail spool additions,
DEED.md mutations (Sprint 06 consumes), decree writes (Sprint 05 consumes),
dispatch outbox additions, freight dir creations. Debounce policy: quiesce
window per file, deterministic ordering (sorted paths, sequence numbers).

## ADRs
`docs/adr/0009-mail-and-watchers.md`, `docs/adr/0010-dispatch-protocol.md`.

Verify: golden mail/dispatch vectors; reaction-table unit tests. Finish:
HANDOFF.md. Commit: `feat(core): mail format, dispatch schemas, watcher
vocabulary (ADR-0009, ADR-0010)`.
