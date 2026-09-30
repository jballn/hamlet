---
role: systems-engineer
sprint: 01
step: 2
---

# 01/02 — `hamlet init` bootstrap + config/.env

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §2 and §11.4, HANDOFF.md
(step 1: tree + sanitizer are ready — use them, don't reinvent).

## Deliverable 1 — `hamlet-core::config`

- `config.json` schema (serde, versioned `config_version`): daemon data dir
  (sled location), podman binary path, base image reference, web listen addr,
  dispatch settings placeholder, log level. Sensible defaults + validation.
- `.env` handling via dotenvy: typed `Secrets` struct (model endpoint, API
  keys) that is NEVER serialized to charter/logs. `.env` created by init at
  mode 0600 with commented placeholder keys (`test-key-do-not-use` style).

## Deliverable 2 — `hamlet init`

Wire `hamlet-cli`: `hamlet init <dir>` → creates the full normative tree via
`HamletTree::ensure`, writes default `config.json`, writes `.env` template
(0600), writes initial `charter.json` (step 3 defines internals — for now
write the empty versioned skeleton `{"charter_version": N}` agreed in
HANDOFF/ADR-0004 when it lands; coordinate via a `charter::empty()` stub you
add and step 3 fills). Idempotent: re-running init on an existing tree is a
no-op that reports what exists; never clobbers charter/config/.env.

## Deliverable 3 — tests

Golden-file test: init a tempdir, snapshot the full tree listing (paths +
modes) and compare against a committed expected listing. Idempotency test.
0600 assertion on `.env`.

Verify: `cargo test --workspace`; `hamlet init /tmp/hamlet-test` manually and
inspect. Finish: HANDOFF.md. Commit:
`feat(core,cli): init bootstrap, config schema, .env secrets (SPEC §2, §11.4)`.
