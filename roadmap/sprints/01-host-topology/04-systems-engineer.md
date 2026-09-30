---
role: systems-engineer
sprint: 01
step: 4
---

# 01/04 — sled telemetry store

Read first: `roadmap/CONVENTIONS.md` §2–§3 (sled is telemetry ONLY, never
orchestration state), `SPEC.md` §11.2, HANDOFF.md (steps 1–3).

## Deliverable — `hamlet-daemon::telemetry`

- Open a sled db under the daemon data dir from `config.json` (not inside the
  `.hamlet/` tree — telemetry is out-of-band; record rationale in ADR).
- Typed metrics API over a key-layout you define: token tallies per agent
  session, API cost accumulation, latency spikes, system event traces
  (append-only trace records). Keys namespaced by commission/uid/session.
- Query API for summaries: per-agent totals, per-commission totals, recent
  trace window. Export function writing a JSON summary into
  `keep/archives/` (used at commission close, Sprint 06 — build the export
  now, wire later).
- Unit tests with a tempdir db: write/read round-trips, summary math, export
  golden file. No orchestration state (slots/commissions) in sled — assert
  this boundary in a test that fails if a charter-shaped key appears.

## ADR

- `docs/adr/0005-sled-key-layout.md`: namespaces, key encodings, retention
  policy (archives export = the durable record; sled may be pruned).

Verify: `cargo test -p hamlet-daemon`; ci-local green. Finish: HANDOFF.md.
Commit: `feat(daemon): sled telemetry store + archives export (ADR-0005)`.
