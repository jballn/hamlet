# ADR-0024 — Musl runtime libraries belong in the daemon apk floor

Status: Accepted
Sprint: 04 · Prompt: qa-engineer finding QA04-05 (fixed by container-engineer)
Amends: ADR-0012 cl. 2 only (cl. 1, 3–5 stand)
Refs: SPEC §4 (base environment), §14 [DEV]; CONVENTIONS §5; ADR-0009 cl. 3,
ADR-0014 (env policy), `defaults/toolchain-pins.toml`

## Context

QA04-05 (sprint 04 QA) proved on a real podman host that the pinned `nvm`
artifact `node-v22.23.3-linux-x64-musl.tar.xz` is a *dynamically* linked musl
binary: `readelf -d` reports `DT_NEEDED` on `libstdc++.so.6` and
`libgcc_s.so.1` (V8's C++ runtime and GCC support). `alpine:3.24` ships
neither, so every parcel's node probe died at the dynamic loader with
`Error loading shared library libstdc++.so.6` → exit 127, and every guarded
provisioning flow (freight, bootstrap-toolchain, provision suites) died at or
before that probe. The apk floor v1 (`ca-certificates curl gzip tar xz`) plus
the blueprint's apk tools install no C++ runtime. The requirement comes from
the **daemon's pin table** (an artifact property), not from any promotable
village policy (ADR-0009 cl. 3), so it cannot be left to a blueprint.

## Decision

1. `BOOTSTRAP_FLOOR` gains **`libgcc`** and **`libstdc++`**; `FLOOR_VERSION`
   bumps **1 → 2**. The bump is what forces a rebuild: the derived tag is
   `v<bp>-f<FLOOR_VERSION>-c<fnv1a64(Containerfile)>` (ADR-0012 cl. 2), so
   every already-built floor-v1 parcel image is superseded rather than
   silently reused.
2. These two are **floor (daemon) packages, never blueprint ones**: a promoted
   blueprint must not be able to drop them by omission, and a blueprint must
   not be able to claim them as a "toolchain choice". They are merged into the
   apk set through the existing sorted-union path (`apk_packages`), so the
   `RUN apk add` line stays deterministic and shell-free.
3. Blueprint `version` stays **1**. ADR-0009 cl. 1 bumps it only on
   promotion; this is a defect fix to the daemon floor, and the floor has its
   own version axis. No DEED promotion event is implied.
4. The floor is the only place runtime-library requirements are declared. A
   future pinned artifact with further `DT_NEEDED` entries gets the same
   treatment — a reviewed floor edit plus a `FLOOR_VERSION` bump, never a
   per-parcel `apk add` (containers run `--user <uid>`, `--cap-drop=ALL`,
   `no-new-privileges`, so `/usr` is not writable at runtime by construction).
5. Loader-hijack remains impossible from inside a parcel: `LD_LIBRARY_PATH` /
   `LD_PRELOAD` / `LD_AUDIT` stay in `FORBIDDEN_ENV_KEYS` (ADR-0014), the
   libraries are root-owned with no group/other write bit, and the container
   user is a slot UID that cannot chown them. Guarded tests assert all three.

## Consequences

- Floor v2 is the first real, verified toolchain floor: node/rust/pixi run in
  a genuine parcel (guarded suite asserts `node --version` == the pinned
  version through the login-shell profile path).
- Image size grows ~10.9 MiB compressed per parcel image — a deliberate cost
  of a working V8 runtime; it is paid once per (blueprint, floor) tag, and the
  tag is localhost-only so nothing is re-pulled.
- Every host with floor-v1 images rebuilds once; old images are orphaned
  (`localhost/` tags are never pruned by the daemon) and can be removed by
  `podman rmi` housekeeping.
- The two `.so`s are now in every parcel rootfs, so "the agent cannot tamper
  with the toolchain" gains a concrete invariant to defend — enforced by
  ownership/mode, not by omission.