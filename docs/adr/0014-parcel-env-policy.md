# ADR-0014 — Parcel env policy: bootstrap-derived prefixes, refused keys

Status: Accepted
Date: Sprint 02 (step 04)
Refs: SPEC §4 (multiplexers in `/home/<uid>/`), §11.4 (secrets); ADR-0009
(`[env]` non-secret, secret-key rejection), ADR-0011 cl. 3 (pkgcache 01777),
ADR-0013 (mirror trust model)

## Context

Blueprint `[env]` is written by operators now and by Sprint 06 promotion later,
so it is an input an agent can influence. The committed default declared
`CARGO_HOME = "/var/pkgcache/cargo"`: an **installation prefix** — executables,
not cache — on a 01777 shared mount, where any UID could plant binaries every
other parcel later runs. Loader variables (`LD_PRELOAD`) are the same attack
without the footprint, and profile values become shell in every login session.

## Decision

1. Bootstrap **derives** the per-UID structural vars and writes them to
   `$HOME/.profile`: `HOME`, `PATH` (tool bin dirs + the image default path),
   `CARGO_HOME`, `RUSTUP_HOME`, `PIXI_HOME`, `NVM_DIR`, `HAMLET_BLUEPRINT`,
   `HAMLET_BLUEPRINT_VERSION`, `HAMLET_TOOLCHAIN_MANIFEST`. Every install root
   is inside `/home/<uid>/` (SPEC §4); only *caches* point at `/var/pkgcache/`.
2. A blueprint `[env]` key in that structural set, or a loader/shell-hijack key
   (`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `ENV`, `BASH_ENV`, `IFS`), is
   **refused** (`ForbiddenEnvKey`) — never silently overridden.
3. Any `[env]` value pointing into a shared mount (`/var/mail`, `/var/freight`,
   `/var/git`, `/var/pkgcache`, `/var/context`) is refused
   (`SharedMountEnvValue`) unless the key is a declared cache key
   (`PIXI_CACHE_DIR`, `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR`) into
   `/var/pkgcache/`. Derived values are re-checked against the same rule.
4. Profile values are single-quoted (`sh_quote`, `'` → `'\''`), so a hostile
   value is inert shell. Keys remain identifier-charset (ADR-0009).
5. Secrets never enter a home: `.env` is not read by bootstrap, DEED.md and the
   manifest record env **keys only**, and the profile states the rule inline
   (injection at session start is Sprint 03, SPEC §11.4).

## Consequences

- `CARGO_HOME` is removed from `defaults/village-alpine.toml`; per-slot cargo
  registries follow (a shared registry mirror is Sprint 07, ADR-0013).
- A promoted blueprint cannot re-introduce a shared executable prefix or a
  loader hijack: bootstrap refuses at provisioning, before any session.
- **Binding on Sprint 03:** sessions must start through a login shell so the
  profile is sourced (`sh -lc`); the container env carries only blueprint
  `[env]` + `HOME` (ADR-0012 cl. 3).
- Step-05 QA: assert the refusals and that no profile value is unquoted.
