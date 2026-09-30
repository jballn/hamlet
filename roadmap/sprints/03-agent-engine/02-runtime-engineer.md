---
role: runtime-engineer
sprint: 03
step: 2
---

# 03/02 — Session spawn & supervision

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §6, ADR-0008, HANDOFF.md
(step 1).

## Deliverable 1 — `FakeAgent` binary

Build the scripted mock per ADR-0008 spec (workspace member under `crates/`
or `tests/support/` — your call, record it): envelope-driven stdin reader,
configurable event emission, on-cue crash/hang/slow-task. This is the primary
test seam for everything below.

## Deliverable 2 — `hamlet-daemon::supervise`

- `start_session(parcel, brief, secrets)`: compose pi argv per ADR-0008
  inside the parcel container via the Sprint 02 exec layer; inject `.env`
  secrets out-of-band (env of the exec, never files in home); capture the
  stdout JSON-RPC stream + writable stdin pipe.
- Health/state tracking → charter status transitions (atomic saves); session
  incarnation counter bumps on restart (SessionId per ADR-0008).
- Restart policy: config-driven limits + backoff; on permanent failure mark
  `crashed`, stamp TITLE.md lineage, write sled trace.
- Graceful stop (drain queue, signal, grace window) and hard kill paths —
  both consumed by Sprint 07 hibernation; expose them as clean APIs now.
- All logic testable against FakeAgent without podman (exec layer seam);
  one guarded live smoke test: real pi (or a real `sh` echo session if pi is
  unavailable in the container — record which) spawn → message → stop.

Verify: `cargo test -p hamlet-daemon` with FakeAgent scenarios (clean run,
crash-restart, restart-limit exhaustion, graceful stop, kill). Finish:
HANDOFF.md. Commit: `feat(daemon): agent session supervision over podman
exec (SPEC §6, ADR-0008)`.
