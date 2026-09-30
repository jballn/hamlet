# Sprint 02 — Container Runtime

**Goal:** The daemon can provision a parcel and run a rootless-podman
container for a given UID slot with exactly the normative mounts, bootstrapped
from a blueprint, with toolchain multiplexers available in user space.

**Scope boundary:** No agents inside containers yet (Sprint 03), no watchers.
Container lifecycle + mounts + blueprints only.

**Exit criteria (SPEC §4, §5):**
- Blueprint TOML schema implemented + parsed in `hamlet-core` (ADR-0006);
  a default blueprint (Alpine + uutils + Pixi/NVM/Cargo) ships committed.
- Podman exec layer: rootless run/exec/stop/kill via tokio process pipes;
  UID mapping per charter slots; no privileged mode anywhere.
- Normative mount table enforced from typed specs — `/var/git/` and
  `/var/context/` ro; home rw per-UID; mail/freight/pkgcache per SPEC §4.
- Parcel provisioning: allocate slot → create parcel dirs (TITLE/DEED/
  PROPRIETOR stubs) → bootstrap home from blueprint.
- Guarded podman integration tests green with `HAMLET_PODMAN_TESTS=1`;
  isolation invariants (ro mounts, no cross-UID home reads) asserted.
- ADRs: 0006 (blueprint schema), 0007 (podman invocation policy).
- Branch merged, tag `sprint/02-container-runtime`.

## Prompt sequence
1. `01-architect.md` — blueprint schema + mount spec types (ADR-0006)
2. `02-container-engineer.md` — podman exec layer (ADR-0007)
3. `03-container-engineer.md` — mounts, permissions, parcel provisioning
4. `04-container-engineer.md` — toolchain bootstrap (Pixi/NVM/Cargo, pkgcache)
5. `05-qa-engineer.md` — isolation & escape suite
6. `06-ci-agent.md` — gate
7. `07-commit-agent.md` — merge & tag
