---
role: runtime-engineer
sprint: 06
step: 4
---

# 06/04 — Agent work-branch loop (push mediation)

Read first: `roadmap/CONVENTIONS.md`, ADR-0012, HANDOFF.md (steps 1–3).

## Deliverable — bundle spool → bare repo

- Agent side (documented convention + brief addendum + a tiny helper script
  shipped into homes via blueprint): agent works in `/home/<uid>/work/`
  (a clone of `/var/git/` — read-only source), commits to its own branch
  `agent/<uid>/<topic>`, produces `git bundle` into the parcel spool dir.
- Daemon side (`hamlet-daemon::ledger` git module): watch spool (reaction
  registry) → validate per ADR-0012 (bundle integrity, author identity =
  slot UID via commit author + daemon-side attribution, branch namespace,
  size caps, no hook execution ever) → fetch/apply into the commission bare
  repo → ack file to the agent's spool → `GitDiffReady` mayor wake (diff
  stat as artifact).
- Conflict policy: branch-per-agent namespaces only; non-fast-forward on a
  shared ref → rejected ack with reason; forced pushes impossible by
  construction (daemon is the only writer).
- Failure paths: corrupt bundle, oversized, foreign-namespace branch,
  spool file from wrong UID (perms audit) — all rejected cleanly + traced.

Verify: unit tests with real local git repos (git is a test dependency of
the HOST, allowed — record in HANDOFF): full bundle round-trip, rejection
matrix; guarded podman e2e: in-container agent clone → commit → bundle →
daemon apply → history intact. Finish: HANDOFF.md. Commit:
`feat(daemon): git bundle push mediation (ADR-0012)`.
