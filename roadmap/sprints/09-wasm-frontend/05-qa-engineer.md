---
role: qa-engineer
sprint: 09
step: 5
---

# 09/05 — Protocol & e2e suite

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.3, ADR-0015, HANDOFF.md
sprint 09 (all steps).

Committed suite:

1. **Protocol conformance**: every ADR-0015 frame type round-trips; unknown
   frames ignored gracefully; sequence-number gaps trigger resync; auth
   rejection without token; malformed command frames → typed error acks,
   no daemon panic (fuzz-ish corpus of wire garbage).
2. **Honesty rules**: spectator connection never receives .env secrets,
   cross-owner mail bodies, or sled raw keys (assert over recorded streams
   of a populated demo village); client-side rendering of owner-only data
   impossible from wire content alone.
3. **State fidelity**: scripted village activity (reuse e2e fake agents:
   commission → work → submit → promote → hibernate) — WS state-delta
   stream reconstructs a view-model identical to CLI `--json` projections
   at 5 checkpoints (cross-surface parity test).
4. **Backpressure**: slow-client simulation — metrics drop, deltas/acks
   never drop; client flood (command spam) rate-limited without starving
   other viewers.
5. **Reconnect**: daemon kill mid-stream → client backoff → resync →
   identical scene (view-model golden), no ghosts.
6. **e2e**: `scripts/e2e-village-web.sh` — headless-browser smoke if
   tooling available in CI, else WS-level simulation harness (record which
   shipped): load page (or sim), drive one full interaction per panel via
   command frames, assert scene/ack outcomes.

Findings → HANDOFF.md. No product fixes. Verdict: `GATE 09/05: PASS|FAIL`.
