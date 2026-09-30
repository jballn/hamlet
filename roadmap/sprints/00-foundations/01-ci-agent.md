---
role: ci-agent
sprint: 00
step: 1
---

# 00/01 — Stand up the CI verification battery

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §3 and §13.

Working in `~/Development/hamlet` (single repo; if no git repo exists yet,
`git init` with `main` as the default branch and record it in HANDOFF.md).

Create the CI battery (GitHub Actions at `.github/workflows/ci.yml`; if the
repo has no GitHub remote, create `scripts/ci.sh` with identical steps and
wire a git pre-push hook instead — decide from `git remote -v` and record the
choice in HANDOFF.md):

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `./scripts/check-deps.sh`
5. guarded podman job: when `podman` is available, run
   `HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored podman` (may be
   a no-op until such tests exist; the job must not fail on their absence)

Also:
- `scripts/ci-local.sh` running the exact same steps, executable.
- `scripts/check-deps.sh`: parses all `crates/*/Cargo.toml` and fails if any
  non-workspace dependency appears outside the locked allowlist (SPEC §3:
  tokio, axum, tokio-tungstenite, serde, serde_json, sled, dotenvy, regex,
  reqwest, notify — plus dev-dependencies allowlist: tempfile, anyhow,
  thiserror, clap, ratatui, crossterm — record this baseline in HANDOFF.md;
  additions require an ADR).
- `.gitignore`: `target/`, `.hamlet/`, `.env`, `*.sled`, `logs/`.

Note: the workspace doesn't exist yet (step 2 creates it) — make the scripts
tolerant of an empty tree (cargo commands run against a minimal placeholder
workspace you create ONLY if needed; step 2 replaces it) and record what you
did.

Verify: run `scripts/ci-local.sh` end to end. Finish: create
`roadmap/sprints/00-foundations/HANDOFF.md`. Commit:
`ci: add verification battery (fmt, clippy, test, deps allowlist, podman guard)`.
