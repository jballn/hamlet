---
role: commit-agent
sprint: 00
step: 4
---

# 00/04 — Foundations merge & tag

Precondition: HANDOFF.md shows gate 00/03 PASS. If FAILED, stop and say so.

In `~/Development/hamlet`:

1. `git status` / `git diff main...HEAD` review — reject anything unexpected
   (secrets, .env, target/, runtime .hamlet/ trees, stray temp files).
2. Ensure history on `sprint/00-foundations` is clean Conventional Commits;
   squash-merge to `main` per CONVENTIONS.md §7.
3. Tag `sprint/00-foundations` on the merge commit.
4. Confirm `scripts/ci-local.sh` green on merged `main` (run it).
5. Append merge record (commit SHA, tag) to HANDOFF.md.

Commit (if any fixups were needed): `chore: sprint 00 fixups`. Then report
the merge SHA and tag.
