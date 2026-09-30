# ADR-0006 — Concurrency-safe atomic writes (unique temp per attempt)

Status: Accepted
Date: Sprint 01 (QA fix pass)
Supersedes: ADR-0004 clause 3 (temp naming only; protocol otherwise intact)
Refs: CONVENTIONS §3 rule 2; SPEC §11.1; QA finding F01-01

## Context

`save_atomic` used the deterministic temp name `.<name>.tmp` (ADR-0004
clause 3). QA F01-01: two concurrent savers share that one temp file —
observed outcomes were spurious `Io { NotFound }` (a peer renames the temp
away mid-write), cleanup races, and worst case a TORN charter published to
`charter.json` (`Corrupt { trailing characters }`) — the master state map
(SPEC §11.1) destroyed. Kill -9 alone was safe; concurrency was not. The
locked dependency stack (SPEC §3) contains no file-locking crate and std
exposes no advisory locks, so flock-style serialization would require a new
dependency plus its own ADR.

## Decision

1. Each save attempt writes to a UNIQUE temp in the same directory:
   `.<name>.<pid>.<serial>.tmp` (process id + process-wide atomic counter).
   No two live attempts — across threads or processes — ever share a temp;
   `rename` stays the only commit point, so the published file is always
   one complete state (last rename wins).
2. `save_atomic` sweeps the legacy deterministic temp (`Charter::temp_path`,
   kept public and unchanged) BEFORE writing: absent → proceed; stale file
   → removed (crash leftovers are consumed by the next save, as before);
   un-sweepable (e.g. a directory squatting the name) → hard `Io` error
   raised before the committed file is touched.
3. `hamlet init`'s artifact writer (`atomic_write_inner`) uses the same
   unique-temp protocol for `config.json` / `.env`.

## Consequences

- Concurrent `save_atomic` (daemon components, daemon + CLI races) can
  never tear the charter or fail spuriously; pinned by
  `concurrent_savers_never_tear_or_fail` (300 raced rounds).
- A SIGKILLed writer can leave a unique-named temp behind; only the legacy
  name is swept, because a live peer's in-flight temp is indistinguishable
  from litter without lock metadata. Accepted: such litter is inert (load
  never reads it) and visible; clean-cycle no-litter contracts stay pinned
  by `no_temp_litter_*` tests.
- The daemon telemetry export writer (ADR-0005) is single-governor today;
  it adopts this protocol if it ever gains concurrent writers.
