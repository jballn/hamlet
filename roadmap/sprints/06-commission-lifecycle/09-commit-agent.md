---
role: commit-agent
sprint: 06
step: 9
---

# 06/09 — Commission lifecycle merge & tag

Precondition: HANDOFF.md shows `GATE 06/08: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, runtime trees, sled dbs,
   test bare repos.
2. Squash-merge `sprint/06-commission-lifecycle` → `main`.
3. Tag `sprint/06-commission-lifecycle`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
