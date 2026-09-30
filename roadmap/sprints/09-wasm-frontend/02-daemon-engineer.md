---
role: daemon-engineer
sprint: 09
step: 2
---

# 09/02 — axum asset serving + WS streams

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.3, ADR-0015, HANDOFF.md
(step 1; sprint 04 axum server; sprint 08 projections).

## Deliverable — `hamlet-daemon::web` (stage 3 half)

- Static asset layers: serve built `hamlet-web` artifacts (wasm, JS glue,
  sprite sheets, tile map) with immutable cache headers keyed by version
  hash; dev mode serving from a build dir.
- WS endpoint (tokio-tungstenite): token auth handshake → full state-delta
  snapshot → live frames per ADR-0015: projection deltas (diffed off the
  reaction stream), metrics ticks (sled → rate-limited batcher), bubbles
  (mail/steer/escalation/ack events with the owner-only body rule), command
  frames → existing command endpoints → ack correlation.
- Fan-out: multiple concurrent viewers (spectator-safe: no secret material,
  no .env, no cross-owner mail bodies — reuse projection sanitization).
- Backpressure: slow clients get dropped frames (metrics) but never dropped
  state-deltas/command acks (record policy); per-connection send queues
  bounded.

Verify: WS integration tests (tungstenite client in-test): auth, resync,
delta correctness under scripted village activity (reuse e2e fake agents),
metrics batching, bubble owner-rule, command round-trip, slow-client
behavior. Finish: HANDOFF.md. Commit: `feat(daemon): asset serving + WS
streams (SPEC §12.3, ADR-0015)`.
