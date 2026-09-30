---
role: commit-agent
sprint: 03
step: 7
---

# 03/07 — Agent engine merge & tag

Precondition: HANDOFF.md shows `GATE 03/06: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, runtime trees, sled dbs.
2. Squash-merge `sprint/03-agent-engine` → `main`.
3. Tag `sprint/03-agent-engine`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
