---
role: daemon-engineer
sprint: 04
step: 2
---

# 04/02 — Watcher core + mail routing

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §8.1, ADR-0009, HANDOFF.md
(step 1; sprint 03 queue API).

## Deliverable — `hamlet-daemon::watch` + `mailroute`

- notify watcher over the whole `.hamlet/` tree, filtered to the ADR-0009
  vocabulary; debounce/quiesce per spec; single dispatcher task feeding typed
  Reactions to registered handlers (mail now; DEED/decree/outbox handlers
  register in later sprints — make registration a clean extension point).
- Mail router: spool addition → parse/validate header → resolve recipient
  UID → charter cross-check (slot exists, session state) → enqueue into the
  Sprint 03 delivery queue (sequential stdin) → ack marker per ADR-0009.
  Undeliverable (no slot, crashed session, overflow) → bounce file per
  ADR-0009 + sled trace. Daemon restart mid-delivery → recover from ack
  state, no duplicates (test explicitly).
- Sending API for in-daemon actors (Mayor, CLI, system notices): write mail
  into a target spool atomically; sender identity is daemon-asserted, never
  caller-claimed.
- Watcher storm resilience: 1k rapid spool writes → all delivered, order
  per-recipient FIFO, bounded memory.

Verify: tempdir-tree integration tests (no podman; FakeRuntime/FakeAgent
sessions). Finish: HANDOFF.md. Commit: `feat(daemon): inotify watcher core +
mail spool routing (SPEC §8.1)`.
