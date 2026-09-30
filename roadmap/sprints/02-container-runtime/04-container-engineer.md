---
role: container-engineer
sprint: 02
step: 4
---

# 02/04 — Toolchain bootstrap (Pixi/NVM/Cargo + pkgcache)

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §4 (base image philosophy)
and §5, HANDOFF.md (steps 1–3).

## Deliverable — blueprint bootstrap execution

`bootstrap_home(parcel, blueprint)`: materialize the blueprint's `[tools]`
into the parcel home BEFORE the first agent session:

- Base image = Alpine + uutils (image ref from blueprint `[meta]`; build or
  pull policy recorded in HANDOFF). No systemd-class init — the container's
  entrypoint is a slim keep-alive the daemon controls (SPEC §4).
- Pixi, NVM, Cargo installed into `/home/<uid>/` (user space, per SPEC §4 —
  agents self-extend from here). Installers must use `market/pkgcache/`
  (mounted `/var/pkgcache/`) as download cache/mirror so re-provisioning is
  offline-friendly and fast.
- `[env]` defaults written to the home profile; secrets from `.env` are
  injected at session start (Sprint 03), NOT baked into homes — assert this.
- Idempotent: re-running bootstrap on an existing home is a verified no-op
  (hash/manifest of installed tools recorded in the parcel's DEED.md
  `### Dynamic Alterations` as the baseline entry).

Guarded podman integration test: provision from `defaults/village-alpine.toml`,
exec `pixi --version`, `node --version` (via nvm), `cargo --version` inside;
second run of pkgcache-dependent install performs zero network downloads
(assert from pkgcache hit instrumentation).

Verify: guarded tests green; ci-local green (skips guarded). Finish:
HANDOFF.md (base image ref + cache policy decisions). Commit:
`feat(daemon): blueprint toolchain bootstrap via pkgcache (SPEC §4, §5)`.
