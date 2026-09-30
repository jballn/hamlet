---
role: container-engineer
sprint: 02
step: 2
---

# 02/02 — Rootless podman exec layer

Read first: `roadmap/CONVENTIONS.md` §5, `SPEC.md` §4, ADR-0007, HANDOFF.md
(step 1: MountSet + Blueprint types exist).

## Deliverable — `hamlet-daemon::podman`

Async wrapper over the `podman` CLI per ADR-0007 (tokio process, Vec<String>
args, never a shell):

- `run_detached(spec)` — start an agent/mayor container: rootless, userns
  mapping so container user = charter UID slot, hostname, resource-limit
  flags (values from config.json placeholders; policy ADR is Sprint 07 —
  wire the flags, defaults conservative), mount set rendered from step 1.
- `exec(container, cmd, stdin)` — piped stdio for interactive sessions
  (Sprint 03 consumes this; build it now with a FakeCommand test seam).
- `stop(container, grace)` / `kill(container)` / `state(container)` →
  typed ContainerState (running/exited/paused/unknown).
- `pull(image)` idempotent, with pkgcache-friendly retry.
- Structured error type capturing podman stderr verbatim; every failure path
  logs the full argv (never env/secrets).

Design for the seam: a `Runtime` trait so `FakeRuntime` (recorded argv, no
podman) powers all non-guarded tests; real podman tests are
`#[ignore]`-guarded per CONVENTIONS.md §5.

Verify: `cargo test -p hamlet-daemon` (FakeRuntime argv goldens); with
podman available run one real `run_detached` + `state` + `stop` smoke test
(`HAMLET_PODMAN_TESTS=1`). Finish: HANDOFF.md. Commit:
`feat(daemon): rootless podman exec layer behind Runtime trait (ADR-0007)`.
