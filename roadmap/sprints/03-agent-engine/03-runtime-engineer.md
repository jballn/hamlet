---
role: runtime-engineer
sprint: 03
step: 3
---

# 03/03 — Sequential delivery queue + divine interruption

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §6 (delivery + interruption),
ADR-0008, HANDOFF.md (step 2).

## Deliverable — per-recipient delivery queue

- One FIFO queue per session. Messages (mail items, Mayor tasking, system
  notices) are enqueued as typed envelopes and written to the session's
  stdin **sequentially**: the next message is written only after the current
  task boundary (agent returns to idle per the JSON-RPC stream). Never
  interleave mid-task. Backpressure: bounded queue depth (config), typed
  error on overflow (routed back to sender as bounce — mail plumbing is
  Sprint 04; expose the hook).
- Persistence: undelivered queue survives daemon restart (recover from
  charter/tree, per file-as-truth — decide location, record in HANDOFF).
- **Divine interruption**: `steer(session, packet)` bypasses the queue and
  injects the `interrupt_and_steer` envelope immediately down the live pipe,
  concurrent with an in-flight task. Correlation id ties the interrupt to the
  resulting stream events. Sources: system | human (CLI `hamlet agent
  steer` — wire the daemon API; CLI surface polish is Sprint 08) | mayor
  (Sprint 05 consumes).
- Race safety: interrupt during queue-drain write must not corrupt framing
  (single writer task owning the stdin pipe; interrupt goes through the same
  writer with priority).

Verify: FakeAgent tests — ordering under load, boundary semantics, interrupt
mid-task with correlation, restart-recovers-queue, overflow bounce. Finish:
HANDOFF.md. Commit: `feat(daemon): sequential stdin delivery +
interrupt_and_steer injection (SPEC §6)`.
