# Shared Conventions (binding on every prompt)

## 1. Repository & paths

- **Single repo (spec + code):** `~/Development/hamlet`
  - `SPEC.md` is law. `MANIFESTO.md` and `DESIGN.md` are non-normative history.
  - Code: Cargo workspace under `crates/` — `hamlet-core`, `hamlet-daemon`,
    `hamlet-cli`, `hamlet-tui`, `hamlet-web`.
  - Docs: `docs/adr/` for decisions; `scripts/` for CI/e2e tooling.
  - The runtime village tree (`.hamlet/`) is created by `hamlet init` inside a
    *target working directory* — never inside the repo itself. Tests use
    tempdirs.

## 2. Architecture rules (violations fail review)

- **Singular governor**: only `hamlet-daemon` spawns processes (podman, pi
  sessions) or mutates a live `.hamlet/` tree at runtime.
- **Purity seam**: `hamlet-core` has no filesystem/process/network side
  effects — it exposes typed operations over injected traits. All core logic
  must be unit-testable without podman or agents.
- **File-as-truth**: any behavior that isn't derivable from the tree +
  charter.json doesn't exist. No hidden in-daemon state (sled is telemetry
  only, never orchestration state).
- **Typed paths**: all `.hamlet/` paths go through the `hamlet-core` tree
  layout module. No ad-hoc `format!`/`PathBuf::push` of village paths.
- **Sanitizer at the boundary**: every name derived from agent, Mayor, or
  webhook input passes the §4 sanitizer before touching disk.
- **Locked dependency stack** (SPEC §3): tokio, axum, tokio-tungstenite,
  serde/serde_json, sled, dotenvy, regex, reqwest, notify. New crates require
  an ADR; `scripts/check-deps.sh` enforces the allowlist in every gate.

## 3. State contract (every charter/state change must satisfy ALL of these)

1. State lives in `BTreeMap`/`BTreeSet` — iteration order is sorted, always.
2. `charter.json` writes are atomic: serialize to temp file in the same
   directory → `fsync` → `rename`. Never write in place.
3. Canonical JSON via serde_json with sorted keys (BTreeMap falls out); a
   `charter_version: u32` field is mandatory; migrations are additive-only
   with explicit version steps.
4. No wall-clock reads inside state derivation. Timestamps are recorded only
   as telemetry/lineage data (TITLE.md stamps, sled), never as state inputs.
5. UID slots (1000, 1001–2000) are permanent per identity; allocation is
   sequential; exhaustion is a hard error, never a wrap or reuse.
6. Restart-resumability: kill the daemon, restart against the same tree →
   identical recovered state (live processes re-attach or are marked crashed).
   Integration tests must cover this from Sprint 01 onward.

## 4. Path/name sanitizer (locked by Sprint 01, ADR-0002)

- Allowlist: `[A-Za-z0-9._-]`, plus a single internal `.`-suffix for file
  extensions; length 1–64; reject `.`, `..`, leading `-`, control chars,
  path separators, and any unicode outside the allowlist.
- Implemented once in `hamlet-core::sanitize` (regex-based per SPEC §3);
  every promotion, mail route, decree target, and webhook filename uses it.
  Golden test vectors committed.

## 5. Containers & agents

- Rootless podman only; invoked via `podman` CLI through tokio process pipes
  (no libpod bindings, no privileged mode, no Docker).
- Mounts exactly per SPEC §4 table; `/var/git/` and `/var/context/` are ro.
- Agent sessions are pi coding-agent processes over `podman exec` stdio;
  mail delivery is sequential per-recipient stdin; interrupts are out-of-band
  JSON-RPC `interrupt_and_steer` packets.
- Container-touching tests are guarded: they run only when
  `HAMLET_PODMAN_TESTS=1` and `podman` is on PATH; otherwise they skip with a
  recorded reason. CI runs them in a podman-capable job.
- **Host preconditions are part of the guard (ADR-0032).** A host on which an
  invariant cannot hold at all (the test user is also the rootless podman
  owner; the village tree would live on a RAM-backed `TMPDIR`) makes the
  suite SKIP with its finding id and the host remedy — never RED, and never
  by weakening an assertion. A guarded suite that skipped every test is
  reported as skipped by `ci.sh`/the e2e harnesses, never as a pass.

## 6. Secrets

- `.env` (mode 0600) via dotenvy; never committed; never echoed into logs,
  charter.json, archives, or test fixtures. Test fixtures use obvious fakes
  (`test-key-do-not-use`).

## 7. Git & commits

- Conventional Commits: `feat(core):`, `feat(daemon):`, `feat(cli):`,
  `feat(tui):`, `feat(web):`, `fix(…):`, `test(…):`, `docs:`, `ci:`,
  `chore:`. One logical change per commit; prompts end with a commit.
- Branch per sprint: `sprint/NN-<slug>` off `main`; squash-merge when the
  sprint's gate prompt passes; tag `sprint/NN-<name>` after merge.
- Never commit: `target/`, `.hamlet/` runtime trees, `.env`, sled databases,
  logs, or anything that looks like a real secret.

## 8. Definition of Done (every build prompt)

- `scripts/ci-local.sh` green: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `scripts/check-deps.sh`.
- New behavior ships with tests in the same commit.
- Docs touched if public API changed (crate rustdoc, `docs/`, ADRs).
- `roadmap/sprints/<NN-slug>/HANDOFF.md` updated: done / decisions (ADR refs)
  / open issues / verify commands.

## 9. Testing doctrine

- Pure logic (sanitizer, charter, slots, decree parsing, blueprint parsing,
  promotion parsing) → `hamlet-core` unit tests with **committed golden
  vectors**.
- Daemon behavior → integration tests against a tempdir `.hamlet/` tree with
  mock seams: a `FakeAgent` (scripted JSON-RPC over stdio) replaces pi; a
  `FakeRuntime` replaces podman where container semantics aren't the target.
- Container semantics → guarded podman tests (§5), including the SPEC §4
  isolation invariants as an explicit adversarial suite.
- Restart-resumability → every sprint adds at least one kill/restart test
  over the state it introduced.
- E2E → `scripts/e2e-*.sh` patterns (init → commission → scripted fake
  agents → vault promotion), run in CI where podman is available.

## 10. ADRs

Any `[DEV]` decision gets a one-file ADR in `docs/adr/NNNN-title.md`:
Context / Decision / Consequences, ≤ 40 lines. Later prompts treat ADRs as
binding; changing one requires a new ADR that supersedes it.
