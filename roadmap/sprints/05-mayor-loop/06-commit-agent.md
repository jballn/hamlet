---
role: commit-agent
sprint: 05
step: 6
---

# 05/06 — Mayor loop merge & tag

Precondition: HANDOFF.md shows `GATE 05/05: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, runtime trees, sled dbs.
2. Squash-merge `sprint/05-mayor-loop` → `main`.
3. Tag `sprint/05-mayor-loop`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
