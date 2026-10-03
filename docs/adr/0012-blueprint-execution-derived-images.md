# ADR-0012 — Blueprint execution: derived parcel images & pinned toolchains

Status: Accepted (cl. 2 amended by ADR-0024, ADR-0025)
Date: Sprint 02 (step 04)
Refs: SPEC §4 (base image), §5 (blueprints "executed by the daemon at parcel
provisioning", format [DEV]); ADR-0009, ADR-0010 cl. 1–2, ADR-0011

## Context

The default blueprint cannot be executed as written: `apk add uutils` exists in
no Alpine 3.2x repo (it is `uutils-coreutils`), `alpine:3.20` went EOL 2026-05,
and apk tools cannot be installed at bootstrap — containers run `--user <uid>`
with `--cap-drop=ALL` + `no-new-privileges`, so `/usr` is never writable.

## Decision

1. `[meta].base_image` is the **parent**; the daemon derives
   `localhost/hamlet/<name lowercased>:v<bp.version>-f<FLOOR_VERSION>-c<fnv1a64(Containerfile)>`,
   probed via `podman image exists`, else `podman build` from a daemon-private
   **0700 temp context** — never a village path: pkgcache is 01777, and a
   writable Containerfile is arbitrary code as the container's root fs.
2. Containerfile = `FROM <parent>` + one `RUN apk add --no-cache <sorted
   union(blueprint apk tools, BOOTSTRAP_FLOOR)>`; **no ENTRYPOINT/CMD/USER** —
   the daemon supplies the keep-alive at `run` (SPEC §4). Floor v1:
   `ca-certificates curl gzip tar xz` (GNU tar: no `..`/absolute members).
3. `pixi|nvm|cargo` install into `/home/<uid>/` via `podman exec` as the slot
   uid. `run` argv gains `-e HOME=/home/<uid>` + `--workdir /home/<uid>` (a
   numeric `--user` has no passwd entry, so `~` and profile sourcing would
   break); `RunSpec::validate` pins both to the §4 home mount.
4. Concrete version/URL/sha256 come from `defaults/toolchain-pins.toml`
   (`hamlet-core::pins`, embedded), matched EXACTLY on `(kind, name,
   version_spec)` — floating specs (`stable`, `22`) are frozen by the table.
   No pin ⇒ `UnpinnedTool`; pin arch must equal `env::consts::ARCH`.
5. Default blueprint fixed: `alpine:3.24`, `uutils-coreutils`, `CARGO_HOME`
   dropped from `[env]` (ADR-0014). DEED.md `### Dynamic Alterations`
   gains a daemon `- baseline · toolchain bootstrap …` entry (image, manifest
   path+sha256, per-tool artifacts+digests, apk layer, env KEYS).

## Consequences

- One build per (blueprint version, floor, definition) per host; a changed
  definition changes the tag. Refreshing a toolchain is a reviewed pin-table edit.
- Sprint 06 may promote a new `version_spec` only once a pin exists, and must
  read the baseline entry as the environment FLOOR, not an agent alteration.
- **Binding on Sprint 03:** sessions start through a LOGIN shell (`sh -lc`) —
  PATH/CARGO_HOME/RUSTUP_HOME/PIXI_HOME/NVM_DIR live in `/home/<uid>/.profile`.
- Step-02 run-argv goldens change; `podman build` joins step-05's audit.
