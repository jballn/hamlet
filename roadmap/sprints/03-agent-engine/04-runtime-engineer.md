---
role: runtime-engineer
sprint: 03
step: 4
---

# 03/04 — JSON-RPC telemetry parse → sled

Read first: `roadmap/CONVENTIONS.md` §2 (agents never self-report), `SPEC.md`
§6/§11.2, HANDOFF.md (sprint 01 step 4 sled API; steps 2–3 this sprint).

## Deliverable — stream telemetry extractor

- Parser task per session consuming the stdout JSON-RPC stream: extract
  token tallies, API cost events, latency spikes, notable trace events
  (session start/stop/crash, interrupt acknowledgments, tool-call markers
  where present). Malformed frames are traced + skipped, never fatal.
- Write through the Sprint 01 telemetry API (namespaced by
  commission/uid/session). No orchestration state in sled; no telemetry in
  charter.
- Live query surface for later stages: `metrics_for(session|uid|commission)`
  used by CLI (08) and WS streams (09).
- FakeAgent emits configurable metric streams — build golden-vector tests:
  exact token/cost totals from a scripted stream, latency-spike detection
  threshold (config), malformed-frame resilience (garbage interleaved),
  high-throughput soak (10k frames) without queue starvation of the delivery
  writer.

Verify: `cargo test -p hamlet-daemon`; ci-local green. Finish: HANDOFF.md
(threshold/latency-spike definition recorded). Commit: `feat(daemon):
daemon-parsed session telemetry into sled (SPEC §6, §11.2)`.
