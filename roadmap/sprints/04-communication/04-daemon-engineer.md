---
role: daemon-engineer
sprint: 04
step: 4
---

# 04/04 — Dispatch outbox/inbox + axum inbound webhooks

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §8.4–§8.5, ADR-0010,
HANDOFF.md (steps 1–2: watcher + reaction registration).

## Deliverable 1 — `hamlet-daemon::dispatch`

- Outbox handler: watch `courier/dispatch/outbox/` → validate against
  ADR-0010 schema → URL allowlist check (config) → reqwest POST (rustls-tls,
  json) → write inbox response file atomically; retry/backoff/dead-letter
  per ADR-0010; secrets resolved from `.env` by key-name reference only.
- Every derived filename passes the sanitizer; correlation ids preserved
  end-to-end; sled trace per attempt.

## Deliverable 2 — `hamlet-daemon::web` (inbound)

- axum server (listen addr from config.json): `POST /webhook/<topic>`
  endpoints; body size cap; JSON validation; auth per ADR-0010 (shared
  secret header from `.env`). Validated events are written as dispatch
  **inbox** files and/or mail to configured recipients (topic → routing
  table in config.json), then trigger the matching Reaction (Mayor-wake hook
  registered but inert until Sprint 05).
- No orchestration logic in handlers: validate → write file → emit reaction.
  The tree remains the truth.

Verify: integration tests with a local test server (axum test client or
ephemeral bind): outbox round-trip incl. retry + dead-letter, inbound
validation rejects (bad auth, oversize, malformed, traversal in topic
names). Finish: HANDOFF.md. Commit: `feat(daemon): webhook dispatch outbox/
inbox + axum inbound endpoints (SPEC §8.4–§8.5)`.
