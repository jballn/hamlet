# ADR-0015 — Session protocol: pi RPC binding, envelopes, interrupt_and_steer

Status: Accepted
Date: Sprint 03 (step 03/01)
Refs: SPEC §6, §14 [DEV]; CONVENTIONS §2–§3, §5, §9; ADR-0004/0005/0010/0011
Note: Sprint 03 prompts cite this as "ADR-0008" — 0008 is claimed; numbers are never reused (docs/adr/README.md).

## Context

SPEC §6 drives pi sessions over `podman exec` stdio: sequential queued mail
plus out-of-band `interrupt_and_steer` (wire schema [DEV], §14). Evidence —
pi 0.99.1, dev host (`pi --help`; docs `rpc.md`/`rpc-commands.md`/`json.md`):
`--mode rpc` speaks strict JSONL (one object per LF); no native
interrupt-and-steer — `steer` only queues; `abort` interrupts but *continues*
queued messages; no startup banner exists (readiness must be probed).

## Decision

1. **Transport**: one dialect — pi RPC JSONL. Argv template:
   `podman exec -i --user <uid>:<uid> --workdir /home/<uid> [-e K=V]…
   <container> pi --mode rpc [--provider P] [--model M] [--continue]`; `-e`
   only for `.env` provider keys + `PI_TELEMETRY=0 PI_SKIP_VERSION_CHECK=1`
   (masked, ADR-0010 cl. 5); single stdin writer; stderr never parsed.
2. **Envelopes** (`hamlet-core::session`, canonical JSONL, `kind` tag):
   queued `mail|tasking|notice` {correlation, text}; out-of-band
   `interrupt_and_steer` {correlation, source: system|human|mayor, priority:
   normal|critical, steering_text}. Correlation `<m|t|n|i>-<seq>-<uid>.<inc>`
   (seq from sled counters, ADR-0005).
3. **Binding**: queued → one `prompt` (`id <corr>:p`), only in running/idle.
   Interrupt busy → `[clear_queue?] + steer(:s) + abort(:a)` (critical
   prefixes `clear_queue`); running/idle → header-marked `prompt`;
   starting/dead → typed refusal. pi rejects mid-stream prompts itself, so
   boundary bugs cannot interleave. Task boundary = `agent_settled` (never
   `agent_end`); readiness = `get_state` probe id `rdy-<uid>.<inc>`.
4. **Identity/status**: `SessionId{uid,incarnation}` (sled `session u64` =
   `inc<<32|uid`); charter v2 adds `sessions` (uid → {id, status,
   restart_count}); the seven SPEC statuses, closed transition table in
   `can_transition_to` (crashed/stopped → starting only via new
   incarnation). Restart keys (`config.json sessions`,
   v3): `ready_timeout_ms` 30000, `stop_grace_ms` 10000, `queue_depth` 64,
   `continue_on_restart` true, `restart.{max_restarts 5, backoff_initial_ms
   1000, backoff_max_ms 60000}`; backoff `min(initial·2ⁿ, max)` over the
   persisted count (exhaustion → permanently crashed; resets on settle).
5. **FakeAgent** speaks exactly this dialect (normative contract:
   `hamlet-core::session` rustdoc) from `crates/hamlet-daemon/src/bin/` —
   no new workspace crate.

## Consequences

- Step 02 extends the exec layer for session argv (env/user/workdir); steps
  03–04 consume the binding + typed event parser (`Other`/`Malformed` keep pi
  upgrades non-fatal); sprints 04/05/07 consume the rest of the vocabulary.
