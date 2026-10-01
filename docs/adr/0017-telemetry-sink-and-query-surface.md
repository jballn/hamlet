# ADR-0017 — Telemetry sink: non-blocking extraction, bounded evidence, query surface

Status: Accepted
Date: Sprint 03 (step 03/04)
Refs: SPEC §6, §11.2, §12.1/§12.3; CONVENTIONS §2, §3 r4/r6, §9; ADR-0005/0016
Supersedes: —

## Context

The step-03 single-writer funnel makes the life task's select loop the only
writer to a session's stdin, so blocking work there starves sequential
delivery and the Divine Interruption — and sled writes are synchronous. A
hostile agent can also flood the stream without bound, and the CLI (Sprint 08)
plus the WS canvas (Sprint 09) need to read metrics back live.

## Decision

1. **Split.** The fold (`metrics::StreamExtractor`) is pure — no I/O, no clock
   reads; every `at_unix_ms` is caller-supplied (CONVENTIONS §3 r4) — and runs
   inline in the life task's stdout arm, so telemetry adds only arithmetic to
   the hot path and can never block a drain or an injection. Every sled write
   belongs to a per-session sink task fed by a bounded channel using `try_send`
   only; one wake folds the whole queue into a single `add_tokens` +
   `add_cost_micro_usd` pair plus the append-only records.
2. **Backpressure.** Additive observations (tokens, cost) coalesce into a local
   accumulator and are retried, so accounting is **lossless** however far the
   sink falls behind. Append-only records (spikes, traces) queue up to 64;
   beyond that the oldest is dropped and the loss counted. Telemetry degrades —
   it never blocks supervision and never corrupts state (ADR-0005 cl. 6).
3. **Bounded evidence.** Per life: 8 malformed-frame traces (then one
   suppression trace), 64 tool-call markers, 32 distinct unknown record types
   (first sighting traced — the pi-version-drift signal), 256 open tool spans
   and 64 pending acks, both FIFO-evicted and counted. Uncapped totals always
   survive in `session-stream-summary`, so flooding can neither grow memory nor
   hide evidence.
4. **Trace split — one source per event.** Lifecycle traces stay in `supervise`;
   `metrics` owns the stream vocabulary (`session-{delivery,interrupt}-acked`,
   `session-tool-call`, `session-agent-retry`, `session-stream-event`,
   `session-stream-malformed[-suppressed]`, `session-stream-unknown-record`,
   `session-stream-summary`). Acknowledgments are traced on success only — a
   failed `response` belongs to the writer side, which owns the drain gate.
5. **Query surface.** `Telemetry::metrics_for(MetricsScope::{Session, Agent,
   Commission, Uid}, window) -> MetricsSnapshot`: serializable, sorted,
   clock-free, no new trees and no key-layout change. `Session` scope is
   session-exact (no agent rollup — traces are agent-keyed); `Uid` crosses
   commissions and so walks the trees, making `Agent` the preferred scope when
   the commission is known. `Supervisor::metrics_for_{session,uid,commission}`
   resolve the commission from the charter.

## Consequences

- The sink flushes at stream end, so a graceful stop checkpoints; a `kill -9`
  may lose the unflushed tail, which ADR-0005 cl. 6 permits and no test assumes.
- `finish` must run on every life-exit path (crash, kill, EOF) — the wrapper
  around the life body guarantees it, so open spans and the summary always land.
- Sprint 08/09 render `MetricsSnapshot` directly; adding a field is additive.
