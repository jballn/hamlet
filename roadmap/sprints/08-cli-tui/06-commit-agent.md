---
role: commit-agent
sprint: 08
step: 6
---

# 08/06 — CLI & TUI merge & tag

Precondition: HANDOFF.md shows `GATE 08/05: PASS`. If FAILED, stop and say so.

1. `git diff main...HEAD` review — reject secrets, token files, runtime trees.
2. Squash-merge `sprint/08-cli-tui` → `main`.
3. Tag `sprint/08-cli-tui`.
4. `scripts/ci-local.sh` on merged main; confirm green.
5. Append merge record (SHA, tag) to HANDOFF.md.

Report the merge SHA and tag.
