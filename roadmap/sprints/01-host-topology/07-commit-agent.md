---
role: commit-agent
sprint: 01
step: 7
---

# 01/07 — Host topology merge & tag

Precondition: HANDOFF.md shows `GATE 01/06: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, .env files, target/,
   runtime .hamlet/ trees, sled dbs.
2. Squash-merge `sprint/01-host-topology` → `main` (CONVENTIONS.md §7).
3. Tag `sprint/01-host-topology`.
4. Run `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
