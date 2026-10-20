# ADR-0040 — Resource bounds & pressure-triggered hibernation policy

Status: Accepted
Sprint: 07-hibernation-census · Prompt: 01-architect (step 01)
Refs: SPEC §4 (container limits), §7 (Mayor loop), §10 (hibernation), §14; CONVENTIONS §2 (singular governor), §3 (state contract); ADR-0010 (podman argv), ADR-0015 (session lifecycle & restart limits), ADR-0027 (context packet)

## Context

SPEC §14 tasks Sprint 07 with resolving resource bounds policy: podman container limit flags, pressure detection inputs, candidate ranking, and the OOM/crash interplay.
Containers without limits can starve the host or trigger uncoordinated kernel OOM kills.
Per CONVENTIONS §2, the daemon is the singular governor and telemetry is strictly separated from state.
The Mayor is the sole authority for agent orchestration decisions.

## Decision

1. **Parcel container limit flags.** `config.json` `resources` (v10 → v11, additive) defines baseline per-parcel limits: `memory` (e.g. `512m`), `cpus` (e.g. `1.0`), `pids_limit` (e.g. `256`), and `cpu_shares` (default 1024, range 2..=262144). These translate directly to `--memory`, `--cpus`, `--pids-limit`, and `--cpu-shares` in podman argv.
2. **Pressure detection inputs.** Telemetry, not state: daemon samples container stats (RSS memory, CPU usage) via podman at `sample_interval_secs` (default 10s) over a rolling window of `sample_window` (default 5) samples, stored in sled (`stats.parcel.<uid>`). Memory pressure triggers when rolling average exceeds `memory_pressure_threshold_pct` (default 85%).
3. **Hard governor rule: daemon suggests, Mayor decrees.** The daemon NEVER autonomously hibernates agents under resource pressure. When pressure is detected, the daemon evaluates candidates and emits a `WakeReason::HibernateCandidate` wake event with structured evidence to the Mayor. Only a Mayor decree (`Decree::HibernateAgent`) or operator CLI (`hamlet agent hibernate <uid>`) may hibernate an agent.
4. **Candidate selection ranking.** Candidates are ranked deterministically by: (a) session status (`Crashed` / `Idle` / `Stopped` first; `Busy` protected), (b) commission status (`WindingDown` before `Active`), (c) absence of unread tasks, (d) idle duration (longer idle = higher score), and (e) memory footprint (higher usage = more reclaimed memory). Ties break deterministically by ascending UID.
5. **OOM / crash policy interplay.** An OOM kill (exit code 137 or OOMKilled flag) consumes consecutive restart budget under Sprint 03 restart policy (`max_restarts`, default 5). TITLE.md logs `crash exit_code=137 reason="OOMKilled"`. When restarts are exhausted, status transitions to `Crashed` and Mayor is woken with `SessionCrashed` evidence to decree stasis or escalation. If repeated OOMs occur under memory pressure, early `HibernateCandidate` suggestion is emitted before budget exhaustion.

## Consequences

- Config schema bumps to `config_version = 11` (additive; v1..v10 parse with defaults).
- `packet.rs` gains `WakeReason::HibernateCandidate` and `EventKind::HibernateCandidate`.
- Mayor retains total executive authority; autonomous daemon kills are architecturally barred.
