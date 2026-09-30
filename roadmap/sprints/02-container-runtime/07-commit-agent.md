---
role: commit-agent
sprint: 02
step: 7
---

# 02/07 — Container runtime merge & tag

Precondition: HANDOFF.md shows `GATE 02/06: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, .env, target/, runtime
   trees, sled dbs, pulled-image artifacts.
2. Squash-merge `sprint/02-container-runtime` → `main`.
3. Tag `sprint/02-container-runtime`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
