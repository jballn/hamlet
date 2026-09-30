---
role: commit-agent
sprint: 09
step: 7
---

# 09/07 — WASM frontend merge & tag

Precondition: HANDOFF.md shows `GATE 09/06: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, built artifacts that
   belong in releases not git (per .gitignore policy), runtime trees.
2. Squash-merge `sprint/09-wasm-frontend` → `main`.
3. Tag `sprint/09-wasm-frontend`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
