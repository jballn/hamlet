---
role: commit-agent
sprint: 10
step: 5
---

# 10/05 — Merge, release candidate, release notes

Precondition: HANDOFF.md shows `GATE 10/04: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, runtime trees, artifacts.
2. Squash-merge `sprint/10-final-verification` → `main`.
3. Tag `sprint/10-final-verification`, then tag the release candidate
   `v0.1.0-rc1` on main.
4. Write `RELEASE-NOTES.md`: stage status (stage 1 daemon+CLI complete,
   stage 2 TUI complete, stage 3 WASM village complete), ADR index (0001–
   0015+), conformance matrix location, soak summary, known limitations
   from docs/BACKLOG.md.
5. Run `scripts/ci-local.sh` on merged main one final time; confirm green.
6. Append merge record + tag SHAs to HANDOFF.md.

Report merge SHA, both tags, and the release-notes path.
