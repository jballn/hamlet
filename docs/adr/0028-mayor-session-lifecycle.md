# ADR-0028 — Mayor session lifecycle: fresh invocation per wake, merged wakes, bounded queue

Status: Accepted
Sprint: 05-mayor-loop · Prompt: 05/02-daemon-engineer
Refs: SPEC §7 (stateless executive, zero-token sleep), §4 (mount table),
§6 (session protocol), §11.4; CONVENTIONS §2, §3 r2/r3/r6, §9; ADR-0011
(manor braces), ADR-0015 (pi RPC binding), ADR-0027 (packet schema),
ADR-0026 cl. 5 (ack-timeout wake)

## Context

Sprint 05 step 01 fixed the *packet*; the process that receives it was
still open. SPEC §7 requires a Mayor that "sleeps at zero token cost", is
"woken on a wake event", and never maintains "a continuous chat thread".
ADR-0015 supplies the decisive finding: pi's `--continue` **resumes pi's own
session history** from the parcel home, and a single pi process answers many
prompts in one session context. So "park a long-lived process and send it a
fresh turn" silently accumulates a thread — precisely what §7 forbids — and
the only ways to prune it are (a) deleting the Mayor's private context from
behind its back or (b) hoping a reset flag exists. Both are mutations of the
agent's own state by the daemon, which file-as-truth and the statelessness
rule both forbid.

Separately, the wake plane needs a bound and a merge rule: an inotify storm
must not grow memory without limit, and five mails waiting at once should
not cost five model turns.

## Decision

1. **Fresh invocation per wake.** One wake = one `pi --mode rpc` process
   spawned over `podman exec`, one readiness probe, **exactly one** prompt
   (the rendered packet), the task boundary (`agent_settled`), stdin EOF,
   then reap. `--continue` is never passed; the argv comes from
   `pi_session_cmd`, and a turn that overruns `mayor.turn_timeout_ms` is
   abandoned (killed), never resumed. Consequence: between wakes the Mayor
   holds **no process**, so zero-token sleep is structural, not a hope; and
   no park strategy can leak history into a turn.
2. **Merged wakes (amends ADR-0027 cl. 2 additively).** A packet still
   carries exactly one artifact; several same-reason events are folded
   *into* it via the new `EventKind::MergedBatch` + `EventArtifact::merged`
   (per-member typed headers, `--- artifact <n> ---` fenced bodies). A wake
   has exactly one cause: mixed reasons are refused
   (`PacketError::MixedWakeReasons`) and wait for their own wake. Refusals,
   never truncation — a batch that would exceed `MAX_ARTIFACT_BYTES` splits
   across further wakes. A one-event batch renders as that plain event.
3. **Bounded, deduplicating queue + strict serialization.**
   `config.mayor.wake_queue_depth` (1..=256, default 16) bounds pending
   events; overflow is a counted refusal, never a silent drop; identical
   artifacts dedupe. Evaluations serialize on an async mutex, so at most
   one Mayor process is alive at any instant.
4. **Manor provisioning.** `keep/census/manor/` is provisioned like a
   parcel for UID 1000: `reserve_mayor`, `MAYOR.md` seeded **once** from
   `defaults/MAYOR.md` (adopted, never clobbered, `create_new` so a planted
   symlink is refused), `home/` + `home/decrees/`, the mail spool, the
   ADR-0011 braces (`apply_manor_home`, 0700 + unshare chown 1000), then
   `RunSpec::for_mayor` (`MountSet::for_mayor`) through the `Runtime` seam.
5. **Config v7 → v8 (additive).** `mayor.wake_queue_depth` and
   `mayor.turn_timeout_ms` join `mayor.ack_timeout_ms`; older documents
   parse with defaults (no rewrite of existing trees).
6. **File-as-truth life.** Each turn writes `charter.sessions[1000]`
   (`Starting` → `Running` → `Busy` → `Idle` → `Stopped`, one incarnation
   per wake); `MayorEngine::recover` heals an orphaned alive record to
   `Crashed` (CONVENTIONS §3 r6). Wake-hook bridges (`freight::MayorWake`,
   `web::WebhookWake`) hand events over fire-and-forget; webhook artifacts
   are loaded from the tree's own inbox envelope, never from memory.

## Consequences

- A wake costs exactly one process spawn; the Mayor cannot accumulate a
  thread even by accident, and a crashed turn leaves a charter trail rather
  than a half-live session.
- Merging trades a little prompt size for far fewer turns at zero token
  cost; the split rule keeps every artifact intact.
- Two Mayor *containers* cannot be distinguished by a live exec: the engine
  owns one session at a time by construction, so a second container start is
  an operator/provisioning concern, not a wake-plane race.
- Sprint 05 step 03 wires the wake triggers (watcher reactions, ack
  timeouts) and decree execution onto these seams unchanged.