---
role: commit-agent
sprint: 04
step: 7
---

# 04/07 — Communication merge & tag

Precondition: HANDOFF.md shows `GATE 04/06: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, .env, runtime trees.
2. Squash-merge `sprint/04-communication` → `main`.
3. Tag `sprint/04-communication`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
