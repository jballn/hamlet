# HANDOFF — Sprint 00 Foundations

## Done

- **01-ci-agent:** CI verification battery stands and is green.
  - `scripts/ci.sh` — the battery (5 steps + e2e sweep, see below).
  - `scripts/ci-local.sh` — executable thin wrapper that `exec`s `ci.sh`, so
    local and CI runs are byte-identical by construction.
  - `scripts/check-deps.sh` — dependency allowlist enforcement (parses
    `crates/*/Cargo.toml`; tolerant of an empty tree).
  - `scripts/hooks/pre-push` — committed copy of the hook; installed into
    `.git/hooks/pre-push` (runs `scripts/ci.sh` on every push).
  - `.gitignore` — `target/`, `.hamlet/`, `.env`, `*.sled`, `logs/` (plus
    `__pycache__/` for the roadmap runner).
  - Minimal placeholder workspace: root `Cargo.toml` (resolver 2) +
    `crates/hamlet-core` stub with one trivial test — created ONLY because the
    tree was empty and the cargo steps needed something to run against.
    **Prompt 02 (architect) replaces it** with the full 5-crate skeleton.
  - Verified: `scripts/ci-local.sh` run end to end → `CI battery: PASS`
    (exit 0). Negative test of `check-deps.sh` (injected `evil-crate`
    dev-dep) → FAIL exit 1, allowed deps (incl. `hamlet-*` path deps and
    `foo.workspace = true` dotted keys) → PASS.

- **02-architect:** workspace skeleton + ADR scaffolding.
  - `docs/adr/` live: `README.md` (when an ADR is required, format, numbering
    rule, supersede rule, index), `0000-template.md` (Context/Decision/
    Consequences, ≤40 lines), `0001-workspace-layout.md` (**Accepted** —
    ratifies the 5-crate split, purity seam, singular governor, dependency
    direction). `0002` is reserved for the sanitizer (CONVENTIONS §4).
  - Placeholder workspace from prompt 01 **replaced**: root `Cargo.toml`
    (resolver 2, `[workspace.package]`, `[workspace.dependencies]` pinned to
    the SPEC §3 locked stack incl. feature sets), 5 members under `crates/`.
  - `hamlet-core`: `lib.rs` crate map (module → SPEC section table, purity
    rules) + 13 stub modules: tree/sanitize/charter/slots/config (sprint 01),
    blueprint/mount (02), session (03), mail (04), decree (05),
    commission/promote (06), stasis (07). Only dependency: `serde` (derive,
    workspace-pinned) — the schema substrate every stub will need.
  - `hamlet-daemon`: `lib.rs` (module map) + stub `main.rs` (prints version)
    + 11 module stubs: telemetry (01), podman (02), supervise (03),
    watch/mailroute/freight/dispatch/web (04, web also 09), mayor (05),
    ledger (06), census (07). Zero dependencies.
  - `hamlet-cli`: stub `hamlet` binary (prints version). `hamlet-tui`,
    `hamlet-web`: stub libs. All zero-dependency.
  - Every stub: doc comment = responsibility + SPEC sections +
    `// TODO(sprint NN)`. **No logic anywhere.**
  - Verified: `scripts/ci-local.sh` → `CI battery: PASS` (fmt, clippy -D
    warnings, test, check-deps OK, podman SKIP).

## Decisions

- **No GitHub remote** (`git remote -v` empty) → per the prompt's decision
  rule: NO `.github/workflows/ci.yml`; instead `scripts/ci.sh` with identical
  steps, wired to a **git pre-push hook**. If a GitHub remote is ever added,
  port `ci.sh` steps 1:1 into a workflow (hook can stay as a fast local gate).
- **Battery steps** (ci.sh): 1) `cargo fmt --all --check`, 2) `cargo clippy
  --workspace --all-targets -- -D warnings`, 3) `cargo test --workspace`,
  4) `./scripts/check-deps.sh`, 5) guarded podman: when `podman` is on PATH →
  `HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored podman` (exits 0
  when no such tests exist — absence never fails); otherwise SKIP with reason.
  Afterwards every existing `scripts/e2e-*.sh` is run (none exist yet).
- **Dependency allowlist baseline** (SPEC §3 + Sprint 00 dev baseline;
  additions require an ADR):
  - Runtime: `tokio`, `axum`, `tokio-tungstenite`, `serde`, `serde_json`,
    `sled`, `dotenvy`, `regex`, `reqwest`, `notify`.
  - Dev-dependencies (additionally allowed there only): `tempfile`, `anyhow`,
    `thiserror`, `clap`, `ratatui`, `crossterm`.
  - `hamlet-*` internal path deps are exempt. `[dependencies]`/
    `[build-dependencies]` must come from the runtime list; dev list is valid
    only under `[dev-dependencies]`.
- Git repo existed on `main` (commit a132596); no `git init` needed. Work
  happens on branch `sprint/00-foundations` per CONVENTIONS §7.
- **Workspace layout → ADR-0001 (Accepted).** All structural decisions from
  prompt 02 are recorded there: 5-crate split, purity seam, singular
  governor, dependency direction (core depends on nobody), workspace-pinned
  SPEC §3 versions.
- **No clap in `hamlet-cli` yet.** clap sits on the *dev*-dependency
  allowlist only; adding it to `[dependencies]` would break
  `scripts/check-deps.sh` (guardrail: stays green). The stub prints its
  version without a parser. Sprint 08 must write an ADR promoting clap (and
  ratatui/crossterm for the TUI) to the runtime allowlist and update
  `check-deps.sh` in the same commit.
- **`reqwest` pinned to 0.12** (`default-features = false`, `json` +
  `rustls-tls`) within SPEC §3's allowed 0.11/0.12 range; recorded in
  ADR-0001's workspace-deps decision.
- **Cargo.lock committed** — workspace builds binaries (daemon, cli), so the
  lockfile is versioned per standard Cargo practice.

## Open issues

- `podman` not installed on this host → step 5 SKIPs here; the guarded branch
  is untested against a real podman until a capable host runs it.
- `cargo test --workspace` currently runs **zero** tests (stubs only, by
  design); Sprint 01 prompt 01 introduces the first real unit tests
  (sanitizer golden vectors, charter atomicity).
- Pre-push hook lives in `.git/hooks` (not versioned); fresh clones must run
  `cp scripts/hooks/pre-push .git/hooks/pre-push && chmod +x .git/hooks/pre-push`.

## Gate 00/03 — Foundations gate (CI Agent)

**Verdict: PASS.** Full battery run via `scripts/ci-local.sh` (delegates to
`scripts/ci.sh`) on branch `sprint/00-foundations` @ `0c21a7d`. Toolchain:
rustc 1.95.0, clippy 0.1.95, rustfmt 1.9.0-stable.

| # | Check | Exact command | Result |
|---|-------|---------------|--------|
| 1 | Format | `cargo fmt --all --check` | **PASS** (no diff) |
| 2 | Lint | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (0 warnings) |
| 3 | Tests | `cargo test --workspace` | **PASS** (0 tests run — stubs only, by design; all 10 targets ok) |
| 4 | Deps allowlist | `./scripts/check-deps.sh` | **PASS** (`OK — all dependencies within the locked allowlist`) |
| 5 | Guarded podman | `HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored podman` | **SKIP** (`podman` not on PATH; step exits 0, never fails) |
| e2e | E2E sweep | `for s in scripts/e2e-*.sh; do …; done` | **N/A** (no `scripts/e2e-*.sh` exist yet) |

Battery terminated with `CI battery: PASS` (exit 0).

### Additional gate verifications

| Check | Command | Result |
|-------|---------|--------|
| All 5 crates are workspace members | `cargo metadata --no-deps --format-version 1` | **PASS** — `hamlet-cli`, `hamlet-core`, `hamlet-daemon`, `hamlet-tui`, `hamlet-web` |
| All 5 crates compile | `cargo build --workspace` + per-crate `cargo build -p <crate>` | **PASS** — each compiles OK |
| ADR files exist | `ls docs/adr/{README,0000-template,0001-workspace-layout}.md` | **PASS** — all three present |
| ADR-0001 matches crate tree | manual diff of ADR-0001 vs `find crates -name '*.rs'` | **PASS** — 5-crate split, purity seam, and core module set (tree/sanitize/charter/slots/config/blueprint/mount/session/mail/decree/commission/promote/stasis) all match the on-disk tree |
| `.gitignore` coverage | `grep -qxF` for `target/`, `.hamlet/`, `.env`, `*.sled`, `logs/` | **PASS** — all five patterns present |
| `git status` clean of forbidden artifacts | `git status --porcelain` | **PASS** — only untracked `roadmap/.runner-state.json` (roadmap-runner state, not a forbidden artifact: no `target/`, `.hamlet/`, `.env`, `*.sled`, or `logs/`) |

No product code was modified by this gate (CI Agent is read-only on product
code). Only this HANDOFF.md gate report was written.

## Verify

```
./scripts/ci-local.sh          # full battery, expect "CI battery: PASS"
./scripts/check-deps.sh        # allowlist only
cargo run -p hamlet-daemon     # stub prints version
cargo run -p hamlet-cli        # stub prints version
ls docs/adr/                   # README, 0000-template, 0001-workspace-layout
git remote -v                  # empty → ci.sh + pre-push hook is the CI
```
