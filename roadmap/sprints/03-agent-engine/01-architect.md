---
role: architect
sprint: 03
step: 1
---

# 03/01 — Session protocol + interrupt wire schema

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §6, HANDOFF.md (sprint 02:
podman exec seam exists). Inspect how pi's JSON-RPC/print modes actually
behave (`pi --help`, docs if available) and record the concrete integration
choice in the ADR.

## Deliverable 1 — `hamlet-core::session`

Typed session protocol: `SessionId` (uid + incarnation counter), message
envelope types for the daemon↔agent channel (queued delivery vs.
out-of-band), session status enum (starting/running/busy/idle/crashed/
stopped/hibernating) for charter. The envelope must make queued mail and
`interrupt_and_steer` **distinguishable by the receiving session** (SPEC §6).

## Deliverable 2 — `interrupt_and_steer` wire schema [DEV → ADR-0008]

Resolve SPEC §14: exact JSON-RPC packet shape (method name, params: steering
text, priority, correlation id, source: system|human|mayor), framing on the
stdin pipe (newline-delimited JSON vs pi RPC frame — decide from pi's actual
protocol and record evidence), and the session-visible semantics (interrupts
current turn; queued deliveries wait for task boundary).

## Deliverable 3 — `FakeAgent` spec

Specify (don't fully build — step 2 builds) a scripted test binary: reads
newline-delimited envelopes from stdin, emits configurable JSON-RPC-ish
event streams (token counts, costs, latencies, crashes on cue, slow-task
hangs). It stands in for pi in all non-guarded tests.

## Deliverable 4 — ADR

`docs/adr/0008-session-protocol.md`: envelope, interrupt schema, pi
invocation form (exact argv template for `podman exec` + pi), restart-policy
config keys.

Verify: type-level tests + golden envelope vectors. Finish: HANDOFF.md.
Commit: `feat(core): session protocol + interrupt_and_steer schema (ADR-0008)`.
