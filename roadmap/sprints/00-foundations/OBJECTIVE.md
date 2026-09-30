# Sprint 00 — Foundations

**Goal:** Make the repo ready to receive the hamlet engine without surprises:
CI gate exists, the 5-crate Cargo workspace skeleton compiles with the planned
module tree (stubs), ADR directory is live, branch model agreed.

**Scope boundary:** No orchestration logic, no podman, no agents, no UI.
Scaffolding + CI only.

**Exit criteria:**
- CI battery (fmt/clippy/test/check-deps) runs on every push and is green;
  `scripts/ci-local.sh` reproduces it locally.
- `crates/{hamlet-core,hamlet-daemon,hamlet-cli,hamlet-tui,hamlet-web}` exist
  with stub module trees per SPEC §3, compile, and are workspace members.
- `docs/adr/README.md` + `0000-template.md` + `0001-workspace-layout.md` exist.
- `scripts/check-deps.sh` enforces the locked dependency allowlist (SPEC §3).
- Branch `sprint/00-foundations` merged to main, tag `sprint/00-foundations`.

## Prompt sequence
1. `01-ci-agent.md` — CI battery + check-deps
2. `02-architect.md` — workspace skeleton + ADR scaffolding
3. `03-ci-agent.md` — gate
4. `04-commit-agent.md` — merge & tag
