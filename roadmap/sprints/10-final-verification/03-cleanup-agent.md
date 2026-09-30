---
role: cleanup-agent
sprint: 10
step: 3
---

# 10/03 — Cleanup pass

Precondition: audit (10/01) and soak (10/02) findings that require CODE
FIXES have been addressed (check HANDOFF.md; if fixes are still pending,
stop and report — cleanup runs on stable code).

Read first: `roadmap/CONVENTIONS.md`, HANDOFF.md sprint 10.

- Remove dead code, `dbg!`/`println!` leftovers (CLI user-facing output
  exempt — it goes through the output layer), commented-out blocks,
  TODO(sprint NN) markers whose sprint has shipped (convert genuine
  future-work TODOs to tracked notes in docs/BACKLOG.md).
- Duplicate test helpers → shared test-support module; stale goldens
  regenerated ONLY where behavior is unchanged (verify via test run).
- Docs consistency: README (write the top-level project README if missing:
  what hamlet is, quickstart init→daemon→tui→web, stage status), crate
  rustdoc, `docs/` index, ADR list complete and numbered without gaps.
- `cargo doc --workspace` warning-free.
- Behavior-freeze rule: if any cleanup would change behavior, STOP and file
  a finding instead.

Verify: `scripts/ci-local.sh` green before AND after (byte-identical test
outcomes). Finish: HANDOFF.md (removed inventory). Commit:
`chore: final cleanup, docs consistency, backlog extraction`.
