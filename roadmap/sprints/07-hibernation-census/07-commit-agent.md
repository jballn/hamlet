---
role: commit-agent
sprint: 07
step: 7
---

# 07/07 — Hibernation & census merge & tag

Precondition: HANDOFF.md shows `GATE 07/06: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, runtime trees, sled dbs,
   stasis fixtures that look like real homes.
2. Squash-merge `sprint/07-hibernation-census` → `main`.
3. Tag `sprint/07-hibernation-census`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
