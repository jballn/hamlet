# ADR-0029 — Decree execution and mayor wake triggers

Status: Accepted (cl. 1's "once per process per decree" superseded by ADR-0030 cl. 4)
Sprint: 05-mayor-loop · Prompt: 05/03-daemon-engineer
Refs: SPEC §6 (supervision), §7 (stateless executive; validation loop),
§10 (stasis); CONVENTIONS §2, §3 r2/r4, §9; ADR-0019 cl. 5 (watcher
vocabulary), ADR-0020 cl. 1 (dispatch outbox), ADR-0026 (decree protocol:
replay algebra, acks, quarantine), ADR-0028 (wake queue)

## Context

ADR-0026 fixed what a decree *is*; this step builds the daemon that decides
what a decree *does*. The Mayor writes files into `manor/home/decrees/` and
reads `<decree_id>.ack` on its next wake — it never runs anything. So the
daemon must be the singular interpreter: watch → validate → replay-check →
execute → acknowledge, with every step observable from the tree alone
(CONVENTIONS §2 file-as-truth).

Three properties have to hold at once:

- **Crash safety.** The daemon may die between admission and execution.
  "Admitted" and "queued" are one atomic charter save (CONVENTIONS §3 r2), so
  a restart always finds either no receipt or a `queued` status — and the
  next scan resumes from there. Every executor is convergent: provision
  adopts, stop is idempotent, enqueue retries, the escalation file and the
  ack are rewritten byte-identically. Re-running an idempotent executor is
  indistinguishable from its first run, so resume needs no special path.
- **Idempotence.** The Mayor re-emits; a bounded wake queue (ADR-0028 cl. 3)
  is the only brake against a spin. A rescan of unchanged state must be a
  no-op: delivered decrees are re-acked `done` without touching the world,
  and an ack-timeout wake fires **once per process per decree** — the memory
  resets on restart, which is the conservative direction (the next process
  re-arms and may wake once more). (Superseded by **ADR-0030 cl. 4**: the
  cadence is now `mayor.ack_retry_ms` and the wake is once per *timeout*,
  which is what the prompt asked for; the anti-spin goal is unchanged.)
- **Honest seams.** Sprint 06 owns deliverable promotion, Sprint 07 owns
  hibernation copies. Those decrees cannot be faked in this step: the seams
  are traits with stubs that acknowledge `deferred` — never a fake `done`.

Separately, four new wake triggers reach the Mayor: mail addressed to UID
1000 (Sprint 04), inbound webhooks, freight audit violations, decree ack
timeouts, and session crashes. Each needs a policy decision about *when* it
wakes — and those decisions belong in `config.json`, not in code.

## Decision

1. **Wake triggers are config-driven.** A webhook event wakes the Mayor only
   when its topic is present in `webhooks.routes` (the routing table that
   already decides where webhook deliveries go): an unconfigured topic never
   reaches the Mayor, so a misfired external hook cannot burn a turn. The
   same rule shapes the other triggers — freight violations wake with the
   audit's own payload as artifact; ack timeouts wake once per process per
   decree (see Context); nothing wakes on topics or reasons the operator did
   not configure.
2. **Crash wake policy.** `mayor.crash_wake` selects which agent-session
   crashes wake the Mayor: `never`, `every`, or `budget_exhausted` (the
   default) — only a crash whose restart budget is exhausted, i.e. one the
   daemon will not revive on its own. The supervisor *notifies* through the
   `CrashWake` seam and never decides: supervision must not know that a Mayor
   exists, and the Mayor must not know how restart policies are implemented.
   The wake artifact carries `uid`, `session`, `restarts`, `permanent`, and a
   scrubbed one-line reason — evidence, not stderr verbatim.
3. **Stasis and promotion go through seams with honest stubs.**
   `hibernate_agent` parks the session through the `Stasis` seam and flips
   the charter lease's `stasis` flag (the slot reads "asleep" to every other
   subsystem); `approve_promotion` / `reject_promotion` run through the
   `Promotion` seam. Both seams ship with recording stubs that acknowledge
   `deferred`: the decree's status stays `queued` — open, not failed — so
   the pass that ships the real implementation executes it on a rescan. A
   stub never acknowledges `done`.
4. **Escalation notices.** An executed `escalate_to_human` writes one
   canonical JSON notice per decree at `manor/escalations/<decree_id>.json`
   — versioned (`ESCALATION_VERSION`), size-capped, evidence refs resolved
   against the tree in pre-flight. The Mayor may write only `decrees/`, so
   the daemon files the notice where a human (CLI, Sprint 08) will look for
   it; replay re-acks without rewriting it. Optionally,
   `mayor.escalation_webhook_url` mirrors the notice to a human channel as a
   dispatch outbox request (ADR-0020 cl. 1): https-only, validated before
   any write, and replayed exactly once because the notice file is the
   checkpoint.

## Consequences

- The decree plane starts with its recovery pass **before** the reaction
  handler attaches (the ADR-0019 precedent): a restart that left a half-
  executed decree finishes it before any newly written decree can interleave,
  and the execution lock means no two passes ever run at once.
- `config.json` gains `mayor.crash_wake` and `mayor.escalation_webhook_url`
  (v8 → v9); both are validated on load, so a bad policy or URL fails boot,
  not the first wake.
- The fake-agent test binary grows an `emit_files` script field (atomic
  temp + rename, as the real Mayor writes) so the full loop — event → wake →
  packet → decree file → execution → ack → sleep — is testable without
  podman.
