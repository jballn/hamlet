# ADR-0018 — Metrics liveness: bounded staleness for the live query surface

Status: Accepted
Date: Sprint 03 (step 03/04, follow-up)
Refs: SPEC §11.2, §12.1/§12.3; CONVENTIONS §2; ADR-0005 cl. 6, ADR-0017 cl. 2
Supersedes: — (refines ADR-0017 cl. 2, which does not change)

## Context

ADR-0017 cl. 2 makes telemetry pushing non-blocking: when the observation
channel is full, additive accounting (tokens, cost) coalesces into the handle's
local accumulator and is retried, so nothing is lost. It did not say *when* the
retry happens. The implementation retried only when the next stream frame
arrived — so a session that went quiet at a task boundary held its final totals
unpublished until the life ended. Measured: a live, idle session answered
`metrics_for` with 4454 of 4500 tokens (`dropped=0` — invisible, not lost), and
the missing tail appeared the instant `stop_session` ran `finish()`. That
contradicts the point of the query surface, which Sprint 08 (`hamlet metrics`)
and Sprint 09 (WS canvas) poll while sessions are alive.

## Decision

1. **Bounded staleness.** Coalesced state must reach sled within
   `metrics::FLUSH_RETRY_INTERVAL` = **100 ms** of the stream going quiet. The
   life task's select loop gains a `FlushTick` arm that calls
   `SessionMetrics::retry_flush()` (non-blocking) and re-pins its timer.
2. **Zero idle cost.** The arm is guarded by `if metrics.has_pending()`, so a
   session with nothing coalesced never polls the timer: the tick exists only
   while there is something to publish. Re-pinning after each fire keeps the
   arm from spinning without yielding (the step-02 elapsed-timer gotcha).
3. **Exactness points.** `metrics_for` is *eventually exact* within 100 ms of
   the stream, and *exactly* exact after `SessionMetrics::finish()` (which
   flushes blocking, since the stream is over and nothing awaits delivery).
   Accounting remains lossless throughout — the bound is on visibility, never
   on correctness (ADR-0017 cl. 2 unchanged).
4. **Guards.** A deterministic unit test for the mechanism
   (`has_pending`/`retry_flush` under a capacity-1 channel), an integration
   test asserting exact totals while a session is **live and idle** (no stop),
   and a source-level ratchet in `scripts/e2e-03-adversarial.sh` (T4) that
   fails if the tick arm or the `retry_flush` call disappears.

## Consequences

- Sprint 08/09 may poll `metrics_for` on a live session at any cadence ≥100 ms
  without special-casing "session still running"; nothing must wait for a life
  to end to see its totals.
- The interval is a constant, not config: it is a liveness bound on telemetry
  publication, not an operator-tuned policy. Promote it to `config.json`
  (additive, version step) only if a stage needs a different trade.
- One extra select arm per live session, polled only while pending state
  exists — no measurable cost on an idle village.
- The underlying race class (asserting on evidence before it has landed) also
  affected three step-05 adversarial scenarios; see HANDOFF finding QA05-05.
