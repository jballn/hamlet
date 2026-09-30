---
role: ci-agent
sprint: 00
step: 3
---

# 00/03 — Foundations gate

Read first: `roadmap/CONVENTIONS.md`, HANDOFF.md from steps 1–2.

Run the full battery (`scripts/ci-local.sh`): fmt, clippy `-D warnings`,
`cargo test --workspace`, `scripts/check-deps.sh`. Additionally verify:

- All five crates are workspace members and compile.
- `docs/adr/{README,0000-template,0001-workspace-layout}.md` exist and
  0001 matches the actual crate tree.
- `.gitignore` covers `target/`, `.hamlet/`, `.env`, `*.sled`, `logs/`.
- `git status` is clean of forbidden artifacts.

Report a single PASS/FAIL table into HANDOFF.md with the exact commands run.
On FAIL: capture minimal failing output, do not fix product code.
