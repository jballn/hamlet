# HANDOFF — Sprint 02 Container Runtime

## Done

- **01-architect:** blueprint schema + typed mount specs (branch
  `sprint/02-container-runtime`).
  - **ADR renumbering (binding for all later steps):** the sprint prompts
    cite "ADR-0006" (blueprint schema) and "ADR-0007" (podman invocation
    policy), but numbers 0006–0008 were already consumed by Sprint 01 and
    ADR numbers are never reused (docs/adr/README.md). The decisions are
    recorded as **ADR-0009** (`docs/adr/0009-blueprint-schema.md`) and
    **ADR-0010** (`docs/adr/0010-podman-invocation-policy.md`), each with a
    mapping note in its header; README index updated. Wherever a later
    prompt says "ADR-0006/0007", read ADR-0009/0010.
  - `crates/hamlet-core/src/blueprint.rs` — full TOML schema per ADR-0009:
    `[meta]` (name = SafeName via `sanitize_name`, version u32 ≥ 1,
    base_image charset-validated), `[[tools]]` (kind pixi|nvm|cargo|apk,
    tool-name charset allows apk `+` e.g. `g++`, optional version_spec
    charset — both shell-safe, never leading `-`), `[env]` (non-secret
    defaults: identifier keys only; keys matching
    `(?i)(key|secret|token|passw|credential)` are hard-rejected per
    SPEC §11.4), `[skills] refs` (SafeNames into `market/skills/`).
    All sections `deny_unknown_fields`; `[meta]` required, rest default
    empty. Private serde wire types (`RawBlueprint`) → validation →
    public typed `Blueprint`.
  - **Immutability encoded structurally:** `Blueprint`/`BlueprintMeta`/
    `ToolSpec` have private fields + accessors only; no setter, no
    `&mut self` method exists. Sprint 06 promotion must construct a *new*
    value with `version + 1` (documented in module rustdoc + ADR-0009 cl. 3;
    a TOML *render* path is deliberately deferred to Sprint 06).
  - **Errors are precise:** every `BlueprintError` variant carries the
    exact field path (`meta.name`, `tools[2].version_spec`, `env.KEY`,
    `skills.refs[1]`) + typed reason; TOML syntax/type/unknown-field
    errors wrap the `toml` crate message (key path + position). Path
    accessor `BlueprintError::path()` for tooling.
  - `defaults/village-alpine.toml` — committed default blueprint per
    SPEC §4: Alpine 3.20 + uutils + git + acl (freight `setfacl`) +
    ca-certificates + curl; Pixi/NVM/Cargo multiplexer entries; `[env]`
    points CARGO_HOME/PIXI_CACHE_DIR/NPM_CONFIG_CACHE/PIP_CACHE_DIR into
    `/var/pkgcache/`; `skills.refs = []`. Embedded in core as
    `DEFAULT_BLUEPRINT_TOML` (`include_str!`) + `Blueprint::default_village_alpine()`.
  - `crates/hamlet-core/src/mount.rs` — SPEC §4 table as a **closed enum**
    `MountSpec { AgentHome{uid}, Mail, Freight, CommissionGit{commission},
    PkgCache, Context, ManorHome }`: variants carry only uid/commission —
    host paths derive exclusively through `HamletTree` accessors (ADR-0003),
    container paths are the fixed spec literals (`/home/<uid>/`,
    `/var/mail/`, `/var/freight/`, `/var/git/` ro, `/var/pkgcache/`,
    `/var/context/` ro, `/home/1000/`). Mounts outside the table are
    unrepresentable (no raw-path constructor exists).
  - `MountSet::for_agent(uid, commission)` → Result; rejects uid outside
    1001–2000 (`MountError::AgentUidOutOfRange`) so non-slot homes can't be
    built. Rows in table order: home, mail, freight, git(ro), pkgcache,
    context(ro). `MountSet::for_mayor()` → ManorHome(/home/1000/) + mail +
    freight + pkgcache + context(ro); **no git mount** — the Mayor is
    multi-commission and receives diffs in §7 context packets (rationale in
    rustdoc; flag to step 02/03 if they need otherwise).
  - `to_podman_args(&tree)` → pure `Vec<String>` of `-v host:container[:ro]`
    pairs, deterministic order, no execution (ADR-0010 cl. 1–2). A village
    root containing `:` fails hard (`MountError::HostPathColon`) — podman
    splits volume values on colons; never quoted/escaped. Helpers:
    `is_writable(set, container_path)` for QA invariant assertions.
  - **Dependency addition (ADR-0009 cl. 4):** `toml 0.8` added to workspace
    deps + hamlet-core; `scripts/check-deps.sh` RUNTIME_ALLOW updated with
    ADR reference. Purity seam untouched — parsing takes `&str`, core
    never reads files.
  - Tests: hamlet-core lib now **56** (was 42; +7 blueprint, +7 mount):
    golden parse of the committed default (asserts every SPEC §4 baseline
    element), full-schema golden, error-path goldens for all 11 semantic
    violations, `deny_unknown_fields` coverage, agent/mayor argv golden
    vectors, ro-mode exhaustiveness, uid-range rejection matrix, colon-root
    fail-hard, table-closure enumeration, immutability-by-replacement doc
    test.
  - Verified: `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS` (fmt, clippy -D warnings, tests, check-deps, e2e).

- **02-container-engineer:** rootless podman exec layer in
  `crates/hamlet-daemon/src/podman.rs` (ADR-0010; prompt ref "ADR-0007").
  - **`Runtime` trait seam** (dyn-compatible via a local `BoxFuture` alias —
    no `async_trait`, stack stays locked): `run_detached(spec, tree)`,
    `exec(container, cmd, stdin)` (buffered), `spawn_exec(container, cmd)`
    → `Box<dyn ExecHandle>` (live piped stdio: `AsyncWrite` stdin,
    `AsyncRead` stdout/stderr, `wait()` — the substrate Sprint 03 drives pi
    sessions over), `stop(container, grace)`, `kill(container)`,
    `state(container)` → typed `ContainerState {Running|Exited|Paused|Unknown}`
    (+ pure `parse_container_state`), `pull(image)` idempotent
    (`image exists` probe first) with bounded linear-backoff retries
    (pkgcache-friendly; attempts/delay configurable, defaults 3 × 2s).
  - **`FakeRuntime`** (pub, in-lib so steps 03/05 tests can use it): builds
    argv through the **same pure builders** as the real runtime, records
    every argv (`argv_log()`), spawns nothing; scripted state/exec output/
    run-id/exec-handle; `fail_next(kind)` failure injection queue for
    rollback tests.
  - **`PodmanRuntime`**: tokio `Command`, piped stdio, `Vec<String>` argv,
    never a shell (ADR-0010 cl. 1); stdin written from a task so large pi
    payloads can't deadlock the pipe; `kill_on_drop` on interactive
    children; `podman_bin` from `config.json`.
  - **`RunSpec`** (typed sources only, ADR-0010 cl. 2): `for_agent(identity,
    uid, commission, blueprint, config)` (mounts from `MountSet::for_agent`,
    hard error outside 1001–2000), `for_mayor(...)` (UID 1000, manor
    hostname, no git bind), image from `BlueprintMeta::base_image`, env from
    `Blueprint::env` (sorted), default keep-alive body `sleep infinity`
    (`with_command` override for step 04). `to_run_argv(tree, bin)` is pure
    + golden-tested: `run -d --name --hostname
    --userns=keep-id:uid=<uid>,gid=<uid> --user <uid>:<uid>
    --security-opt=no-new-privileges --cap-drop=ALL --memory/--cpus/
    --pids-limit`, then `MountSet::to_podman_args`, then `-e K=V`, image,
    command. Re-validates every hand-settable field at build time (uid
    policy, image/cmd/env charset, resource limits).
  - **Flag-injection guards:** container names pass the §4 sanitizer before
    reaching positional argv slots (`--all`, `-a`, `../x` rejected,
    golden-tested); commands must be non-empty, control-char-free, and must
    not start with `-`; images charset-checked.
  - **`PodmanError`** (ADR-0010 cl. 5): kind (`Spawn`/`NonZeroExit`/
    `NoSuchContainer`/`Mount`/`InvalidSpec`) + full argv + podman stderr
    **verbatim**; `Display` renders all three so every failure path logs the
    argv. Env values are masked via `redacted_argv` (`-e K=V` →
    `K=<redacted>`, also `--env=` form) — argv skeleton stays auditable,
    env/secrets never logged (CONVENTIONS §6). `exec` maps podman's own
    exit-125/"no such container" to errors; in-container exit codes flow
    back in `ExecOutput` untouched.
  - **config.json v1→v2 (additive):** new `resources` placeholder section
    (`memory` `512m`, `cpus` `1.0`, `pids_limit` 256 — conservative until
    the Sprint 07 policy ADR), regex-validated because values land verbatim
    in argv; `#[serde(default)]` so v1 documents still parse unchanged (no
    silent rewrites); `CONFIG_VERSION` bumped to 2; core golden + CLI
    `tests/golden/config.json` updated; migration test committed.
  - Tests: daemon lib now **27** (was 10; +17 podman): agent/mayor run argv
    goldens, lifecycle argv goldens, forbidden-flag audit across **all**
    spawn paths (no `--privileged`, `--userns=host`, `--pid=host`,
    `--network=host`, socket mounts, `--authfile`, `sh -c`; hardening flags
    asserted present — feeds step 05 item 3), ro-invariant argv audit
    (`/var/git/:ro`, `/var/context/:ro`, exactly 6 agent rows),
    name/command/image injection matrix, uid policy matrix, state-parse
    golden, redaction golden (env masked, stderr verbatim), failure
    injection, invalid-spawn-nothing, pull idempotency, scripted
    state/exec, FakeExecHandle stdio piping, resource-limit tamper
    rejection.
  - **Guarded real-podman test** `crates/hamlet-daemon/tests/
    podman_guarded.rs` (`#[ignore]` + `HAMLET_PODMAN_TESTS=1` + podman-on-
    PATH guard, skip reason recorded; ci.sh step 5 filter matches
    `podman`): pull → `run_detached` → `state`=Running → exec `id -u`
    asserts container UID == charter slot 1001 → exec `touch /var/git/…`
    must fail (ro invariant) → `stop` → `state`=Exited → cleanup `rm -f`.
    **Not executed on this host — podman is not installed here** (guard
    skips with recorded reason); must be run on the podman-capable CI job.
  - `tokio` added to hamlet-daemon deps (workspace, +`io-util`,`time`
    features) — locked-stack crate, no ADR needed.
  - Verified: `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS`.

- **03-container-engineer:** parcel provisioning + enforcing host permission
  model (branch `sprint/02-container-runtime`).
  - **ADR-0011 (`docs/adr/0011-host-permission-model.md`) amends ADR-0010
    cl. 3 — binding for steps 04/05:** `--userns=keep-id:uid=<uid>` is
    REMOVED from every run argv. Keep-id maps the single host daemon user
    onto *each container's own* slot UID, so daemon-owned files appear
    owned by the container user in every container — ownership can never
    distinguish slots on SHARED mounts (`/var/mail/`), which SPEC §4
    requires ("cross-UID read blocked by spool perms"). Containers now run
    with podman's DEFAULT rootless private userns (subuid mapping,
    identical in every container) + `--user <uid>:<uid>`; per-UID host
    ownership is established via `podman unshare chown <uid>:<gid> --
    <paths>` (the prompt's "chown via podman unshare" option — the daemon
    is root-equivalent in its userns, never rootful on the host). No
    `--userns` flag on any run argv, ever; step-05 QA audit must assert
    that (the forbidden-flag audit in `podman.rs` already does).
  - **Host permission table (ADR-0011 cl. 3)** in
    `crates/hamlet-daemon/src/hostperm.rs`: parcel `home/` 0700 uid:uid ·
    `courier/mail/<uid>/` 0700 uid:uid (spool root 0755) ·
    `courier/freight/` 0710 daemon (ACL-ready — Sprint 04 `setfacl`
    sandboxes layer on top, nothing blocks them) · commission `git/` +
    `tavern/gossip/` 0755 daemon (all slots read, none write — ro mount is
    belt, these are braces) · `market/pkgcache/` 01777 daemon (sticky
    shared cache) · parcel dir/docs 0755/0644 daemon. `HostPermissions`
    runs through the `Runtime` seam (chown argv recorded + golden-tested
    like every podman call); uid-policy refusals fire before any side
    effect.
  - **`Runtime` trait extended** (podman.rs): `remove(container, force)`
    (`podman rm [--force]`, rollback/orphan prevention) and
    `unshare_chown(uid, gid, paths)` — both with pure argv builders
    (`remove_argv`, `unshare_chown_argv`): paths after a `--` separator,
    each validated absolute/UTF-8/control-char-free/never leading `-`;
    injection matrix golden-tested. `FakeRuntime` gains **verb-targeted
    failure injection** `fail_next_for("run"|"unshare"|"rm"|…, kind)` —
    fails exactly one podman subcommand of a multi-call flow (the seam the
    rollback tests drive); the generic `fail_next` queue still works.
  - **`provision_parcel`** (`crates/hamlet-daemon/src/census.rs`,
    `Provisioner::provision_parcel(identity, commission, blueprint)` →
    `ParcelLease`; blueprint is an explicit parameter until the
    register→blueprint binding exists, Sprint 03+ [DEV]): the prompt's 5
    steps — slot allocation (hard error on exhaustion) → parcel dirs +
    stub `TITLE.md` (trigger + unix ts as LINEAGE data only, CONVENTIONS
    §3 r4) / `DEED.md` (ends with an empty `### Dynamic Alterations`
    block, SPEC §9.4 parse target) / `PROPRIETOR.md` (seeded verbatim from
    `guild/register/<identity>.md` — new typed tree accessor
    `HamletTree::register_brief`; missing/non-regular brief = typed
    pre-flight refusal, zero side effects) → ADR-0011 permissions →
    `pull` + `run_detached` (`MountSet::for_agent`) → charter lease +
    `save_atomic` LAST (disk never sees a lease whose container failed).
    Charter also records `commissions[n]=Open` + `agents[identity]` ref
    (validate() invariants pass before save).
  - **Rollback** (`Undo`): every created dir tracked at the outermost
    granularity (pre-existing state is NEVER touched — crash remnants are
    adopted, per ADR-0007 lstat discipline: stubs `create_new`-written,
    existing regular files kept verbatim); a started container is
    `rm --force`d; incomplete unwind → typed `ProvisionError::
    RollbackFailed { cause, failures }`. **Adversarial find (fixed):**
    rollback must survive its own permission model — after step 3 the home
    is 0700 subuid-owned, i.e. unreadable to the daemon's host user, so
    removal would be locked out; `Undo::execute` first re-claims chowned
    paths via `podman unshare chown 0:0 -- …` (uid 0 in-ns = daemon host
    user), then removes. Golden-tested (reclaim argv present, ordered
    after container kill).
  - **Crash convergence / restart-resumability** (CONVENTIONS §3 r6):
    re-provisioning the same identity adopts intact state (idempotent slot,
    stubs never clobbered) and a FRESH `Provisioner` on the same tree
    recovers byte-identical charter state and continues sequential
    allocation — both integration-tested. A live remnant container makes
    `run_detached` fail on the taken name; rollback never removes
    containers it didn't start (re-attach = Sprint 03 supervision).
  - Tests: daemon lib now **35** (was 27; +6 hostperm, +2 podman
    unshare/remove goldens + verb injection); new integration suite
    `crates/hamlet-daemon/tests/provision.rs` (**14**: happy path with
    full host-state + argv-sequence assertions, adoption/idempotency,
    restart recovery, 4 rollback paths (run/chown/save-squat/rm-failure →
    RollbackFailed), pre-existing-dir preservation, home-inside-remnant-
    parcel reclaim, missing-brief/symlink-brief/symlink-TITLE refusals,
    exhaustion hard error, sequential multi-parcel slots); core 58
    (register_brief accessors + escape tests).
  - **Guarded podman suite** `crates/hamlet-daemon/tests/
    provision_guarded.rs` (`#[ignore]` + `HAMLET_PODMAN_TESTS=1` +
    podman-on-PATH, skip reason recorded; ci.sh step-5 filter matches):
    provisions TWO real parcels end to end, then asserts INSIDE the
    containers: `id -u` == slot; `/var/git` + `/var/context` writes fail
    (ro); home rw (`touch` succeeds AND the file appears on the host bind
    source) and `stat -c %u /home/<uid>` == slot uid; another parcel's
    home not reachable; own mail spool rw while cross-UID spool read is
    blocked BY PERMS in both directions (the ADR-0011 invariant keep-id
    could never give); pkgcache rw; freight ACL-gated (0710) until Sprint
    04; no `/run/podman`, `/var/run`, `/run/docker.sock` inside; after
    teardown: host home 0700 and **subuid-owned (owner != daemon euid)**,
    freight 0710, git 0755, pkgcache 01777. **Not executed on this host —
    podman is not installed here** (guard skips with recorded reason);
    first real run is the podman-capable CI job.
  - **Step-02 golden updates (forced by ADR-0011):** run argv goldens
    (agent + mayor) no longer contain the keep-id flag; the
    forbidden-flag audit now additionally asserts NO `--userns` flag on
    any run argv and covers the two new spawn paths (`rm`, `unshare`).
  - Verified: `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS` (guarded podman step SKIPs on this host, recorded).

- **04-container-engineer:** blueprint toolchain bootstrap via pkgcache
  (branch `sprint/02-container-runtime`).
  - **Base image policy (the prompt's "build or pull policy") — ADR-0012:**
    `[meta].base_image` is the PARENT, not the parcel image. The daemon
    derives `localhost/hamlet/<name lowercased>:v<bp.version>-f<FLOOR_VERSION>-c<fnv1a64(Containerfile)>`
    (default blueprint today → `localhost/hamlet/village-alpine:v1-f1-cb4bce3d7d446249d`),
    probed with the new `Runtime::image_exists` and otherwise built with the
    new `Runtime::build` (`podman build --tag … --file … <ctx>`, pure argv
    builder `build_argv` + golden). Derived tags are `localhost/`-scoped ⇒
    **never pulled**; only the parent goes through the existing idempotent
    `pull`. Containerfile = `FROM <parent>` + one `RUN apk add --no-cache
    <sorted union(blueprint apk tools, BOOTSTRAP_FLOOR)>` and **no
    ENTRYPOINT/CMD/USER** — the daemon keeps supplying the slim keep-alive
    (`sleep infinity`) at `run`, so there is no init of any class (SPEC §4;
    asserted in the golden + the guarded image suite). Floor v1 = `ca-certificates
    curl gzip tar xz` (GNU tar on purpose: strips absolute member paths,
    refuses `..` members — extraction safety rests on it, not on busybox tar).
    The build context is a daemon-private **0700 temp dir**
    (`build_context_dir`, per-village fingerprint, removed after build) —
    NEVER a village path: `market/pkgcache/` is 01777, and a Containerfile any
    agent can rewrite is arbitrary code as the container's root fs.
  - **Three default-blueprint fixes forced by reality (ADR-0012 cl. 5):**
    `alpine:3.20` → **`alpine:3.24`** (3.20 went EOL 2026-05: no more apk
    security updates); `uutils` → **`uutils-coreutils`** (`uutils` exists in
    no Alpine 3.2x repo — verified against dl-cdn.alpinelinux.org; 3.24 ships
    0.11.0); `CARGO_HOME = "/var/pkgcache/cargo"` **removed** from `[env]`
    (an installation prefix — executables — on a 01777 shared mount is a
    cross-UID code-execution surface; ADR-0014). Core goldens updated
    (`default_blueprint_parses` now asserts the fixes AND that no structural
    key is blueprint-declared).
  - **Cache policy (the prompt's "download cache/mirror") — ADR-0013:**
    `market/pkgcache/dist/` is the artifact mirror, created host-side by
    bootstrap at **01777** (sticky: every slot publishes, none clobbers).
    Every artifact runs **stage privately → verify → use**: HIT = `cp
    /var/pkgcache/dist/<f> $HOME/.hamlet/stage/<f>`; MISS = `curl -fsSL
    --retry 3 -o <stage> <https url>`; then `sha256sum <staged>` compared to
    the committed pin — mismatch aborts (`DigestMismatch`) BEFORE any
    extract/exec/publish. Publish is `ln <staged> /var/pkgcache/dist/<f>`:
    a hard link refuses an existing name instead of following a planted
    symlink and publishes exactly the verified inode; failure is non-fatal.
    Modes are normalized before publishing (0644 archives / 0755 binaries) —
    `ln` shares the inode, so a 0600/0700 entry would lock the NEXT parcel
    out of the mirror (found + fixed here; regression-tested). Staging is
    `rm -r -f`'d after extraction (~200 MB per parcel reclaimed). Host-side
    probes use `symlink_metadata`: only a regular non-empty file is a hit.
  - **Pin table (new committed default + new core module):**
    `defaults/toolchain-pins.toml` → `hamlet_core::pins` (`ToolchainPins`,
    `ToolPin`, `ArtifactPin`, `ArtifactFormat`, `PinError` with field paths,
    `DEFAULT_TOOLCHAIN_PINS_TOML` embedded). Exact `(kind, name,
    version_spec)` match freezes floating specs: pixi `0.34.*` → **0.34.0**,
    node `22` → **22.23.3** (+ nvm **0.40.8**), rust `stable` → **1.98.1**
    (rustup-init 1.29.1). sha256 for all 7 artifacts computed from the bytes
    actually downloaded 2026-09-30 and cross-checked against upstream
    manifests (nodejs `SHASUMS256.txt`, `rustup-init.sha256`, the
    `xz_hash` fields of `channel-rust-stable.toml`); arch pinned to x86_64 and
    checked against `env::consts::ARCH`. **No pin ⇒ `UnpinnedTool` refusal**,
    never a guessed version/URL. Node uses the **musl** unofficial build —
    nodejs.org's official linux-x64 tarball is glibc-linked and cannot run on
    Alpine.
  - **Installers (all user-space, `/home/<uid>/`, per SPEC §4):** pixi →
    `$PIXI_HOME/bin/pixi`; nvm → `$NVM_DIR` with node materialized into
    nvm's OWN `$NVM_DIR/versions/node/v22.23.3/` layout (so `nvm use 22` and
    a bare `node` both resolve offline); rust → `rustup-init -y
    --no-modify-path --default-toolchain none` (multiplexer only, no
    unverified dist fetch) + pinned `rustc`/`cargo`/`rust-std` component
    tarballs extracted `--strip-components=2` into
    `$HOME/.rust/toolchains/hamlet-1.98.1` and `rustup toolchain link` +
    `rustup default`. **Verified on this host against real rustup 1.29:** the
    three-strip-2 layout links cleanly, `default` accepts a linked toolchain,
    and both are idempotent on re-run (the earlier `dest = <root>/lib` for
    rust-std was WRONG — `<root>/lib/lib/rustlib/…`; fixed + regression test).
  - **Env layer — ADR-0014:** bootstrap derives the per-UID structural vars
    (`HOME`, `PATH` = tool bin dirs + `IMAGE_DEFAULT_PATH`, `CARGO_HOME`,
    `RUSTUP_HOME`, `PIXI_HOME`, `NVM_DIR`, `HAMLET_BLUEPRINT{,_VERSION}`,
    `HAMLET_TOOLCHAIN_MANIFEST`) into `$HOME/.profile`, then the blueprint's
    non-secret `[env]` passthrough. A blueprint key in the structural set, or
    a loader/shell-hijack key (`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`,
    `ENV`, `BASH_ENV`, `IFS`), is **refused**; so is any value pointing into
    a shared mount unless it is a declared cache key (`PIXI_CACHE_DIR`,
    `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR`) into `/var/pkgcache/`. Profile values
    are single-quoted (`sh_quote`), so a promoted hostile value can't become
    shell in later sessions. **Secrets are never baked in:** `.env` is not
    read by bootstrap, DEED/manifest record env KEYS only, and the profile
    says so inline (integration + guarded assertions).
  - **Idempotence:** the desired state is a pure function of `(uid,
    blueprint, pins, floor)` — canonical JSON `$HOME/.hamlet/toolchain.json`
    + exact `.profile` bytes. `bootstrap_home` `cat`s both back, and on a
    byte match runs only the three `--version` probes → `BootstrapReport
    { no_op: true, downloads: 0, steps_executed: 0 }`; a wiped/corrupt home
    falls through to a full run (convergence, CONVENTIONS §3 r6). The
    manifest's sha256 is recorded in DEED.md `### Dynamic Alterations` as the
    `- baseline · toolchain bootstrap …` entry (image, manifest path+sha256,
    per-tool artifacts+digests, apk layer, env KEYS) — appended once per
    digest, atomically (ADR-0006), host-side (DEED is daemon-owned 0644; the
    home is not writable from the host, ADR-0011).
  - **Plan/execution split (CONVENTIONS §2/§9):** `plan_bootstrap(uid, bp,
    pins, cached)` is PURE and golden-tested (48-step cold sequence, argv
    goldens, warm-mirror variant, determinism, per-uid); only `Bootstrapper`
    touches fs/podman, through the `Runtime` seam. No shell anywhere: every
    step renders argv (`env K=V …` prefix for the structural vars), and
    `validate_inner_argv` stops the inner command becoming an `env` option.
  - **Wired into provisioning (SPEC §5 "executed … at parcel provisioning"):**
    `provision_steps` now resolves the image (`ensure_image`) → runs the
    container with `RunSpec::with_image(derived)` → `bootstrap_home` →
    charter save LAST. New `ProvisionError::Bootstrap`, new
    `Provisioner::provision_parcel_with_report` → `Provisioned { lease,
    image, bootstrap }` (the report IS the pkgcache hit instrumentation);
    `provision_parcel` still returns the lease. Bootstrap failure rolls the
    whole lease back (container `rm --force`, dirs reclaimed+removed, charter
    never sees the lease) — integration-tested.
  - **`RunSpec` amendment (ADR-0012 cl. 3):** `run` argv gains
    `-e HOME=/home/<uid>` + `--workdir /home/<uid>`, both derived from the
    typed mount table (`podman::home_container_dir`) and **pinned there by
    `validate()`** (a hand-moved workdir/HOME is rejected). Without them a
    numeric `--user` has no passwd entry, so `HOME` is unset/`/` and neither
    `~` nor `~/.profile` resolves — the whole env layer would be dead code.
    Step-02 run-argv goldens updated accordingly.
  - **`Runtime` seam extended:** `image_exists(image) -> bool` (probe without
    pull) and `build(tag, containerfile, context)`, both with pure argv
    builders and `FakeRuntime` impls (so the forbidden-flag audit now covers
    12 spawn paths incl. `build`). `FakeRuntime` gains `script_exec_for
    (matcher, ExecOutput)` (first match wins) + `stdin_log()` — bootstrap
    needs per-command answers (`cat`, `sha256sum`, probes) and the stdin log
    proves what bytes reached a home. `script_pinned_digests(fake, uid, pins,
    home_digest)` is the documented test-support helper.
  - Tests: core lib **65** (was 58; +6 pins, +1 tree mirror-accessor
    traversal matrix); daemon lib **63** (was 35; +26 bootstrap, +2 podman
    HOME/workdir + with_image); NEW integration suite
    `crates/hamlet-daemon/tests/bootstrap.rs` (**18**: cold full flow + DEED
    baseline, host-never-writes-the-home + exact stdin bytes, secrets-never-
    reach-the-home, verified no-op (6 offline execs, zero downloads), fresh-
    bootstrapper convergence, warm mirror ⇒ 0 downloads / 7 hits still
    verified, poisoned mirror ⇒ refusal with nothing extracted/published/
    claimed, symlinked mirror entry ⇒ miss never followed, publish failure
    non-fatal, ensure_image build-once + private context + never pulls the
    derived tag, build failure typed + cleaned up, failing step, failed
    probe, malformed DEED, symlinked DEED, unpinned tool ⇒ no side effects,
    provisioning rollback on bootstrap failure, baseline never duplicated);
    `provision.rs` (**14**, unchanged count) updated for the new tail: verbs
    `unshare unshare image run exec…`, derived image in the run argv, DEED =
    stub + baseline, no env values in DEED.
  - **Guarded podman suite** `crates/hamlet-daemon/tests/bootstrap_guarded.rs`
    (2 tests, `#[ignore]` + `HAMLET_PODMAN_TESTS=1` + podman-on-PATH, skip
    reason recorded; ci.sh step-5 filter matches `podman`): (a) the derived
    image is slim/init-free — no ENTRYPOINT/CMD, no systemd/openrc/runit,
    GNU tar + xz/curl/gzip/setfacl/git/uutils present, `sha256sum` checked
    FUNCTIONALLY (busybox has no `--version`), and every tool bootstrap's argv
    names exists; (b) provision parcel A from `defaults/village-alpine.toml`
    ⇒ 7 downloads, then INSIDE via `sh -lc` (the login-shell contract):
    `pixi --version`, `node --version` (v22.23.3), `nvm --version` (0.40.8),
    `nvm which 22`, `cargo --version` + `rustc --version` (1.98.1), the
    derived env vars per-UID, no `.env` secret anywhere in the home, no
    secret-shaped export in the profile, toolchain files owned by the slot
    uid, staging dropped, PID 1 is the keep-alive, `/var/git` + `/var/context`
    still ro, no cross-UID home read, no infra sockets, mirror holds 7 regular
    files; then re-bootstrap A ⇒ `no_op`, `downloads == 0`, `steps_executed
    == 0`; then provision parcel B ⇒ **`downloads == 0`, `cache_hits == 7`,
    still 7 digest verifications** (the prompt's zero-network assertion, from
    the instrumentation) with B's toolchain working and A/B mutually
    unreadable. Host-side: one baseline entry per DEED, no secret in it,
    mirror entries regular + non-stub; teardown reclaims the subuid homes so
    the tempdir can go. **Not executed on this host — podman is not installed
    here** (guard skips with recorded reason); first real run is the
    podman-capable CI job. Cold run downloads ~200 MB once per village.
  - Verified: `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS` (guarded step SKIPs here, recorded).

- **05-qa-engineer:** committed adversarial isolation & escape suite
  (branch `sprint/02-container-runtime`). Extends — never duplicates — the
  step 02/03/04 suites, per the step-04 handoff note.
  - **NEW FakeRuntime suite** `crates/hamlet-daemon/tests/adversarial_isolation.rs`
    (6 tests, all green on this host, ~28 s dominated by the 1000-agent run):
    (1) **full-flow argv audit** — a COMPLETE provisioning run's recorded
    argv (unshare/image/run/40+ bootstrap execs) is swept for privilege
    (`--privileged`, any `--userns`, `--pid/--network/--ipc=host`,
    `--authfile`, `--remote`), infrastructure sockets/dbus, host-path volume
    escapes (`:/run`, `:/etc`, …), `sh -c`; the run argv must carry
    no-new-privileges + cap-drop=ALL + exactly the 6 SPEC §4 rows with
    `/var/git/` + `/var/context/` `:ro` and host paths inside the tree;
    every exec argv path token is contained to `/home/<uid>` +
    `/var/pkgcache` (step-04 note item 4, now pinned at provision level).
    (2) **chown policy audit** — every `podman unshare chown` targets a slot
    uid or the rollback reclaim `0:0` (and only home/spool); shared/ro
    mounts (git, gossip, freight, pkgcache, mail ROOT) are never chowned
    away from the daemon → the host-side "perms braces" for ro dirs
    (0755/0710 daemon-owned: the mapped subuid has no write bit) are
    asserted on the real fs too. (3) **failure injection at EVERY provision
    step** (`unshare`, `image` probe, `pull`, `build`, `run`, bootstrap
    `exec`) → no orphan parcel/spool/commission dirs, charter.json
    byte-identical, started containers force-removed, and the retry re-uses
    slot 1001 (no slot leak, no wrap). (4) **remnant-parcel adoption** —
    pins the F-02-01 behaviour (below) and proves convergence: retry adopts
    the orphaned stub verbatim. (5) **slot exhaustion for real** — 1000
    agents provisioned through the full flow: slots exactly 1001..=2000
    sequential/permanent, every identity→slot mapping checked; the 1001st
    is a typed `SlotError::Exhausted` with ZERO podman calls, charter bytes
    unchanged, no parcel 2001; existing identities stay deployable.
    (6) **bootstrap integrity** — a fake secret in the real `.env` never
    appears in any recorded argv, any exec stdin payload (the bytes that
    land in homes), or ANY persisted file in the tree (SPEC §11.4).
  - **NEW guarded podman suite** `crates/hamlet-daemon/tests/escape_guarded.rs`
    (2 tests, `#[ignore]` + `HAMLET_PODMAN_TESTS=1` + podman-on-PATH, skip
    reason recorded; ci.sh step-5 filter matches): **(a) escape suite**
    (fast — PARENT image only, no ~200 MB bootstrap; isolation must not
    depend on toolchains): provisions two parcels' worth of host state via
    the real `HostPermissions` seam, then from inside container A: ro
    escapes — direct writes, shell redirection, hardlink OUT of ro,
    symlink INTO ro (dir + file links), unprivileged `mount -o remount,rw`,
    chmod/chown on ro content — all must fail and leave nothing behind;
    cross-UID — B's home, B's spool (read AND write), spool FORGERY
    (`mkdir /var/mail/9999` in the 0755 root), freight list/write (0710
    until Sprint 04) — all fail both directions while own home/spool/
    pkgcache stay rw; no infrastructure — podman/docker/dbus/systemd socket
    matrix, no container tooling (`command -v podman docker nerdctl ctl`),
    no `DOCKER_HOST`/`CONTAINER_HOST` env; privilege state — `CapEff`/`CapPrm`
    all-zero + `NoNewPrivs: 1` from `/proc/self/status`; userns —
    `/proc/self/uid_map` parsed: no rootful identity mapping, the daemon's
    host uid must NOT fall inside the mapping (ADR-0011 disjoint subuid
    policy), and host-side owners prove it: home/spool owners are subuids,
    DISTINCT per slot, never the daemon uid, exactly `uid_map(slot)`, while
    git/gossip/freight/mail-root/pkgcache stay daemon-owned. **(b)
    bootstrap integrity**: full provisioning with a fake `.env` secret →
    secret absent from the home AND the pkgcache mirror (in-container
    `grep -rI`), absent from `podman history`/`inspect` of the DERIVED
    image (image layers), no `.env`-shaped file in home/mirror, no
    `Containerfile` anywhere under the village tree (ADR-0012
    root-fs-as-code guard), no secret in any host-readable village file.
    **Not executed on this host — podman is not installed here** (guard
    skips with recorded reason); first real run is the podman-capable CI job.
  - **NEW harness** `scripts/e2e-02-isolation.sh` (auto-picked-up by ci.sh):
    T1 runs the FakeRuntime suite; T2 source ratchets — no podman/docker
    API crates in `Cargo.lock`, `hamlet-daemon/src` never references
    `env_file()`, forbidden-flag literals confined to `podman.rs` (a new
    product file mentioning `--privileged`/`--userns=host`/… fails the
    gate until reviewed); T3 committed goldens are privilege/socket/
    secret-free; T4 guarded escape suite behind a **subuid preflight**
    (`/etc/subuid` range ≥ 2001 for the daemon user + `podman info`
    rootless=true) so a misconfigured host fails loudly BEFORE burning a
    cold image pull. Skips with recorded reason when not podman-capable.
  - **Findings filed (below): F-02-01 (observation, pinned by test) and
    F-02-02 (test-code defect in the step 03/04 guarded suites — fixed by
    QA in-place; test harnesses are QA artifacts, product code untouched).**
    No product-code invariant violations found: every adversarial run of
    items 1–7 either passes or matches documented ADR behaviour.
  - Verified: `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS` (incl. `e2e-02-isolation: PASS`; guarded steps SKIP
    on this host, recorded).

## Decisions

- ADR-0012 — blueprint execution: derived parcel images (parent =
  `[meta].base_image`, tag `localhost/hamlet/<name>:v<bp>-f<floor>-c<fp>`,
  apk union + floor, no ENTRYPOINT/CMD, private 0700 build context),
  home-installed multiplexers, `HOME`/`--workdir` on every run argv, and the
  committed pin table as the version source. Default blueprint fixed to
  `alpine:3.24` + `uutils-coreutils`, `CARGO_HOME` dropped.
- ADR-0013 — pkgcache trust model: `market/pkgcache/dist/` mirror at 01777,
  stage-privately → sha256-verify → use, `ln` publish, `symlink_metadata`
  probes, host never writes a home, byte-comparison idempotence.
- ADR-0014 — parcel env policy: bootstrap-derived per-UID prefixes in
  `$HOME/.profile`; structural + loader-hijack keys refused; shared-mount
  values refused except declared cache keys into `/var/pkgcache/`; values
  single-quoted; env recorded as KEYS only.
- **ADR renumbering (extends the step-01 note):** step 04 consumed 0012–0014,
  which later prompts cite for their own decisions. `docs/adr/README.md` now
  carries a binding mapping table: Sprint 06 "ADR-0012" (commission state
  machine/git mediation), Sprint 07 "ADR-0013" (stasis + resource policy),
  Sprint 08 "ADR-0014" (CLI/API contract), Sprint 09 "ADR-0015" (WS protocol)
  → take the next free number (0015+) when written.
- ADR-0011 — host permission model & subuid userns mapping (amends
  ADR-0010 cl. 3: keep-id removed; default rootless private userns +
  `--user`; `podman unshare chown` for per-UID ownership; mode table).
  Prompt ref: "ADR-0007 addendum" → recorded against ADR-0010 per the
  renumbering note, as its own ADR per the supersede/amend rule.
- ADR-0009 — blueprint TOML schema, env secret-key rejection, structural
  immutability, `toml` crate addition (prompt ref: "ADR-0006").
- ADR-0010 — podman invocation policy: CLI-only via tokio pipes, argv
  `Vec<String>` never a shell, typed-source args, rootless userns/uid
  mapping flags (`--userns=keep-id:uid=<uid>,gid=<uid>` + `--user
  <uid>:<uid>`), forbidden-flag list, `:ro` volume rendering, colon fail-hard
  (prompt ref: "ADR-0007").
- **[DEV] step 03:** register brief location = `guild/register/<identity>.md`
  (typed accessor `HamletTree::register_brief`); a missing brief is a hard
  pre-flight refusal (SPEC §5: register + blueprint define deployability).
  `provision_parcel` takes the `Blueprint` explicitly — identity→blueprint
  resolution (register front-matter or Mayor decree payload) is Sprint 03+
  [DEV]. Provisioning records `commissions[n]=Open` + `agents[identity]`
  alongside the lease (validate() requires the commission ref). Stub
  builders (`title_stub`/`deed_stub`/`proprietor_stub`) are pub + pure for
  golden reuse (Sprint 06 parses the `### Dynamic Alterations` block;
  Sprint 07 reuses TITLE lineage format).
- **[DEV] step 02:** config.json `resources` section (v2) as the
  placeholder source for `--memory/--cpus/--pids-limit`; defaults
  conservative (512m / 1.0 / 256). Binding resource policy ADR remains
  Sprint 07 — this only wires the flags. Container hostname convention:
  `parcel<uid>` (agents) / `manor` (Mayor). Detached keep-alive body:
  `sleep infinity` (busybox-safe on Alpine 3.20; step 04 may override via
  `RunSpec::with_command`). `exec` treats exit 125 as a podman-level error
  (podman's own convention), all other codes pass through in `ExecOutput`.

- **[DEV] step 04:** toolchain versions/URLs/digests live in
  `defaults/toolchain-pins.toml` (embedded via `include_str!`), NOT in
  blueprints — blueprints declare intent (`kind`/`name`/`version_spec`), the
  pin table freezes bytes. Adding a tool = adding a pin (reviewed commit).
  Refreshing `stable`/`22` = editing the table; until then parcels keep the
  pinned versions (reproducibility over freshness).
- **[DEV] step 04:** DEED baseline entry shape (`- baseline · toolchain
  bootstrap · uid <n> · image <ref>` + 2-space-indented `  - manifest:`,
  `  - tool: <kind>/<name> <spec> → <version> · <role>@<ver> sha256 …`,
  `  - image layer (apk):`, `  - env keys:`) is golden-tested in
  `bootstrap.rs`; Sprint 06's `promote` parser must consume exactly this and
  treat it as the environment FLOOR, not an agent alteration.
- **[DEV] step 04:** session shell contract — the toolchain env lives in
  `/home/<uid>/.profile`, so **Sprint 03 must start pi sessions through a
  login shell** (`sh -lc …`); `podman exec` alone inherits only the container
  env (blueprint `[env]` + `HOME`). Guarded assertions use `sh -lc`.
- **[DEV] step 04:** `Runtime::exec` stays env-less (no `-e` on exec): the
  bootstrap argv carries its own `env K=V …` prefix. If Sprint 03 needs env on
  exec, extend `exec_argv` rather than passing `sh -c`.

## Findings — step 05 QA (isolation & escape suite)

- **F-02-01 · OBSERVATION (minor; behaviour PINNED by test; no product
  change made — QA never fixes product code)** · rollback tracks
  DIRECTORIES only, not the stub FILES it wrote inside a pre-existing one.
  - Where: `crates/hamlet-daemon/src/census.rs:326-360` (`Undo` /
    `Undo::execute` — `created_dirs` is the only fs tracking) with the stub
    writes at `census.rs:539-551` (`write_stub`).
  - Repro: `mkdir -p <village>/.hamlet/keep/census/parcel1001` (crash
    remnant), inject a `run` failure (`FakeRuntime::fail_next_for("run",…)`),
    `provision_parcel("scribe", …)` → committed test
    `failure_inside_remnant_parcel_leaves_adoptable_stubs_and_retry_converges`
    (`tests/adversarial_isolation.rs`) pins the outcome.
  - Expected (strictest reading of step-05 item 5): a failed provision
    leaves ZERO artifacts of THIS run. Actual: slot/dir/container/lease all
    unwind cleanly, but `TITLE.md`/`DEED.md`/`PROPRIETOR.md` created by the
    failed run survive inside the pre-existing parcel dir. Task item 5
    enumerates orphan "slots/dirs/containers" — none of those leak, and the
    retry adopts the stubs verbatim (convergent, non-destructive;
    chain-of-title lineage from the first attempt is preserved). Hence
    OBSERVATION, not a gate failure.
  - SPEC citation: CONVENTIONS §3 (file-as-truth, restart-resumability),
    ADR-0007 adopt-don't-destroy discipline; SPEC §10 (chain of title).
  - Suggested disposition (product owners): if byte-clean rollback is
    wanted, `write_stub` should report "created by this run" and `Undo`
    remove exactly those files (never pre-existing ones).
- **F-02-02 · TEST-CODE DEFECT in the step 03/04 guarded suites — would
  false-fail the FIRST real podman CI run; fixed by QA in place (test
  harnesses are QA artifacts; no product code touched).**
  - Where: `crates/hamlet-daemon/tests/provision_guarded.rs:197` and
    `crates/hamlet-daemon/tests/bootstrap_guarded.rs:447` (formerly
    `for sock in ["/run/podman", "/var/run", "/run/docker.sock"]`).
  - Repro (podman-capable host): `HAMLET_PODMAN_TESTS=1 cargo test
    -p hamlet-daemon --test provision_guarded -- --ignored podman` → the
    probe `ls /var/run` inside alpine:3.24 exits **0**, because Alpine
    baselayout ships `/var/run` as a symlink to `/run`, which exists in the
    image → `assert_ne!(code, 0)` fails regardless of isolation state.
  - Expected: probes assert the SPEC §4 invariant — no host infrastructure
    SOCKET reachable ("cannot reach the podman socket or any host
    infrastructure socket") — not the absence of standard directories.
    Actual (pre-fix): directory-existence probe guaranteed to false-fail.
  - Fix (committed): both suites now probe concrete socket paths
    (`/run/podman/podman.sock`, `/run/docker.sock`, `/var/run/docker.sock`,
    `/run/dbus/system_bus_socket`, `/var/run/dbus/system_bus_socket`);
    `escape_guarded.rs` additionally covers `/run/systemd/private`,
    container-tooling absence and endpoint env absence.
  - Caveat: no podman on this host — the failure mode is established
    statically from the Alpine baselayout; the fixed probes are correct
    under either directory behaviour.
- **No product-code invariant violations found** across task items 1–7:
  every adversarial probe (full-flow argv audit, chown policy, six-point
  failure-injection matrix, real 1000-agent exhaustion, secret scan)
  either passes or matches documented ADR behaviour.

## Open issues

- Commit message deviates from the prompt's literal `(ADR-0006, ADR-0007)`
  → uses the real numbers `(ADR-0009, ADR-0010)`; numbers 0006/0007 are
  taken (see renumbering note above). Step 02's commit likewise cites
  ADR-0010, not the prompt's "ADR-0007".
- Real-podman tests (step 02 smoke + step 03 provisioning suite) are
  committed but **never executed** (no podman on this host — guard skips
  with recorded reason). First real run: the podman-capable CI job or
  step 05's host. Step 02's "expect keep-id to need working subuid
  ranges" note is RESOLVED by ADR-0011: keep-id is gone; the default
  rootless subuid mapping + `podman unshare chown` needs a working
  /etc/subuid range (≥ 2000 ids) instead — the guarded suite asserts the
  subuid ownership it produces (home owner != daemon euid).
- **Step 04 (toolchain bootstrap): DONE — the ADR-0011 constraint was
  honoured:** parcel `home/` is 0700 SUBUID-owned, so bootstrap writes it
  ONLY from inside the container (`podman exec … tee <path>` as the slot uid;
  integration test asserts the host-side home stays empty and no host path
  ever appears in an in-container argv). Nothing chmod/chowns a home back to
  the daemon user. The daemon's only host-side writes are
  `market/pkgcache/dist/` (01777) and DEED.md (0644, atomic). Same discipline
  still applies to mail spools (Sprint 04 writes via unshare/in-container).
- Step 05 QA: the forbidden-flag audit now also asserts the ABSENCE of any
  `--userns` flag (ADR-0011) and covers `rm`/`unshare`/`build` spawn paths
  (12 argv recorded); the guarded provisioning + bootstrap suites already
  encode the cross-UID spool/home, ro-mount, no-socket and toolchain-ownership
  invariants — extend, don't duplicate. New escape surface from step 04, all
  already asserted somewhere and worth an adversarial pass:
  (1) `podman build` context location (must stay outside the village tree —
  a writable Containerfile is root-fs-as-code); (2) the world-writable mirror
  (poisoned artifact ⇒ `DigestMismatch`; planted symlink ⇒ miss, never
  followed; publish via `ln` so an existing name is refused not dereferenced);
  (3) blueprint `[env]` as an injection channel (structural/loader keys
  refused, shared-mount prefixes refused, profile values single-quoted);
  (4) bootstrap argv containment (nothing writes outside `/home/<uid>/` +
  `/var/pkgcache/dist/`; no `sh -c`; inner command can't be an `env` option);
  (5) unpinned tool ⇒ refusal with ZERO side effects.
- **Step 05/06 note — the guarded suites have never run:** no podman on this
  host (steps 02, 03, 04, 05 all skip with a recorded reason). Step 05 adds
  `escape_guarded.rs` to that list (2 tests: the fast parent-image escape
  suite + the cold ~200 MB bootstrap-integrity suite). Highest-risk
  unexecuted assertions, in order: (a) `nvm --version` / `nvm which 22` under
  **busybox ash** login shells (nvm is a shell function; if ash can't source
  `nvm.sh`, either add `bash` to `BOOTSTRAP_FLOOR` + bump `FLOOR_VERSION`, or
  drop nvm-function assertions and keep `node --version`); (b) `rustup
  toolchain link`/`default` inside the container (verified working against
  real rustup 1.29 on the HOST with the exact extracted layout, and
  idempotent on re-run — but the toolchain there is musl and could not be
  exec'd on this glibc host); (c) `apk add uutils-coreutils` on alpine:3.24
  (package existence verified against the repo index, not by a real build);
  (d) `podman build` accepting our argv shape; (e) **step 05 addition:** the
  escape suite (like the step-03 suite) assumes `podman exec` inherits the
  container's `--user 1001` — if exec defaulted to image root, the `id -u`
  and cross-UID probes would fail LOUDLY (which is the correct QA outcome;
  the fix would then be an explicit `--user` on `exec_argv`, ADR-0010). A
  first guarded run needs network + ~200 MB of downloads; expect the cold
  run to be slow (the escape suite itself is cheap: parent image only).
- Step 06 (gate): `defaults/toolchain-pins.toml` is a NEW committed default
  alongside `village-alpine.toml`; `hamlet-core::pins` is a new public module
  (core lib.rs table updated). `scripts/check-deps.sh` needs no change (no new
  crates: `toml`/`serde_json`/`regex` were already allowed).
- Step 02 (`hamlet-daemon::podman`): done — see above. Step 03 consumed
  `Runtime::run_detached` + failure injection for provisioning rollback
  (now verb-targeted via `FakeRuntime::fail_next_for`); step 05's argv
  audit can reuse `no_forbidden_flags_in_any_recorded_argv`.
- Sprint 03 note: `spawn_exec` returns a live `ExecHandle`; if supervision
  needs richer scripted sessions (interleaved stdin/stdout), extend
  `FakeRuntime::set_exec_handle` rather than bypassing the trait.
- `Blueprint` → TOML render (promotion write-back) deferred to Sprint 06
  (ADR-0009 consequences).

## Verify

```sh
cargo test -p hamlet-core            # blueprint + pins + mount + tree goldens
cargo test -p hamlet-daemon          # podman/hostperm/bootstrap goldens
                                     #   + provision & bootstrap suites
cargo test -p hamlet-daemon --test adversarial_isolation
                                     # QA 05: full-flow argv audit, chown
                                     #   policy, 6-step failure injection,
                                     #   real 1000-agent exhaustion (~28 s),
                                     #   .env secret sweep
./scripts/e2e-02-isolation.sh        # QA 05 harness: T1 suite + source
                                     #   ratchets + goldens + guarded preflight
cargo test -p hamlet-daemon --lib bootstrap::
                                     # plan/profile/manifest/DEED/env goldens
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon -- --ignored podman
                                     # real smoke + provisioning isolation
                                     # + derived-image & toolchain bootstrap
                                     # + QA escape suite (ro/symlink/cross-UID
                                     #   /socket/userns) & image-layer secret
                                     #   integrity (podman host only; skips here)
./scripts/ci-local.sh                # full battery incl. check-deps
```

## Gate 02/06 — CI battery (ci-agent)

**Precondition note:** HANDOFF.md never received a literal `GATE 02/05: PASS`
line — the QA step committed its suite (`2179fda test(daemon): QA 02/05
adversarial isolation & escape suite`) and its findings section concludes
"**No product-code invariant violations found** across task items 1–7"
(F-02-01 dispositioned OBSERVATION; F-02-02 a test-harness defect fixed in
place by QA). Treated as PASS; recorded here as a process observation.

Battery run on this host (`./scripts/ci-local.sh`, exit 0, `CI battery: PASS`):

| # | Check | Result | Detail |
|---|-------|--------|--------|
| 1 | `cargo fmt --all --check` | **PASS** | no diff |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | zero warnings |
| 3 | `cargo test --workspace` | **PASS** | 24 suites ok — 209 passed, 0 failed, 6 guarded-podman ignored |
| 4 | `scripts/check-deps.sh` | **PASS** | allowlist intact (tokio axum tokio-tungstenite serde serde_json sled dotenvy regex reqwest notify toml) |
| 5 | guarded podman tests (`HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored`) | **SKIP** | podman not installed on this host; ci.sh step 5 records `SKIP: podman not available` — guard skips with recorded reason per CONVENTIONS §5. The 6 `#[ignore]d` tests (podman_guarded 1, provision_guarded 1, bootstrap_guarded 2, escape_guarded 2) remain **never executed** — must run on the podman-capable CI job (carried open issue from steps 02–05). |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | boundary is type-enforced |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | incl. T5b read-only refusal, T6 usage-exit-2 |
| 8 | `scripts/e2e-02-isolation.sh` | **PASS** | T1 suite + source ratchets + goldens; guarded preflight SKIP (no podman) |

### ADR conformance (prompt refs ADR-0006/0007 → real ADR-0009/0010, see step-01 renumbering note)

- **ADR-0009 (blueprint schema): MATCHES implementation.**
  `deny_unknown_fields` on all four wire structs (blueprint.rs:301,313,320,328);
  secret-key rejection regex `(?i)(key|secret|token|passw|credential)`
  (blueprint.rs:110); `DEFAULT_BLUEPRINT_TOML = include_str!(defaults/
  village-alpine.toml)` (blueprint.rs:75) and the file is committed;
  `toml` in check-deps.sh RUNTIME_ALLOW with ADR-0009 citation; structural
  immutability holds — `rg "&mut self" crates/hamlet-core/src/{blueprint,mount}.rs`
  matches only the doc-comment that asserts its absence.
- **ADR-0010 (podman invocation policy): MATCHES implementation**, incl. the
  ADR-0011 amendment of cl. 3:
  - cl. 1 CLI-only/argv-only: `RunSpec::to_run_argv` builds `Vec<String>`
    (podman.rs:567), `PodmanRuntime` spawns via tokio `Command`, no shell;
  - cl. 3 as amended: run argv carries `--user <uid>:<uid>` and **no
    `--userns` flag at all** (builder comment podman.rs:583–586; absence
    pinned by the forbidden-flag audit over all 12 spawn paths);
  - cl. 4 hardening present verbatim: `--security-opt=no-new-privileges`,
    `--cap-drop=ALL` (podman.rs:592–593);
  - cl. 2/6: volumes only via `MountSet::to_podman_args`; env sorted from
    validated `Blueprint::env`.

### Forbidden-string sweep

`rg -n "privileged|podman.sock|docker.sock" crates/hamlet-daemon` → 25 hits,
**all** either (a) tests asserting absence (`tests/adversarial_isolation.rs`,
`tests/escape_guarded.rs`, `tests/provision_guarded.rs`,
`tests/bootstrap_guarded.rs`; inline `#[cfg(test)]` modules — podman.rs hits
all > line 1716, bootstrap.rs hits all > line 2299) or (b) the podman.rs
module rustdoc (lines 4, 23) *stating the prohibition*. **Zero product-code
usage** of `--privileged` or any socket mount. Requirement satisfied.

GATE 02/06: PASS

---

## GATE 02/07 — Merge & tag record (commit-agent)

- **Precondition:** `GATE 02/06: PASS` verified in this HANDOFF (line 775).
- **Diff review (`git diff main...HEAD`):** 35 files, +14541/−29 — all
  source, tests, defaults, ADRs, roadmap, scripts. No secrets, no `.env`,
  no `target/`, no runtime `.hamlet/` trees, no sled dbs, no binaries, no
  pulled-image artifacts (only image references are `docker.io/library/
  alpine:3.24` in the committed blueprint and derived `localhost/` tags in
  ADR-0012 — never pullable). `crates/hamlet-cli/tests/golden/config.json`
  v2 carries only the placeholder `resources` section. Untracked
  `roadmap/.runner-state.json` left unstaged (runner scratch, not branch
  content).
- **DoD pre-check (CONVENTIONS §8):** `scripts/ci-local.sh` on the
  pre-merge sprint tree → `CI battery: PASS`.
- **Squash-merge:** `sprint/02-container-runtime` (tip `6e02a57`) → `main`
  as **`f973675`** (`f973675c0ea759e43c3a09419959809a26643a2b`) —
  `feat(core,daemon): sprint 02 container runtime — blueprint, podman exec
  layer, parcel provisioning, toolchain bootstrap`. Merged tree verified
  byte-identical to the sprint branch tip (`git diff` empty).
- **Tag:** `sprint/02-container-runtime` (annotated) at `f973675`.
- **Post-merge CI on `main`:** `scripts/ci-local.sh` → **`CI battery:
  PASS`** (fmt, clippy `-D warnings`, 209 workspace tests, check-deps,
  e2e-01 ×2, e2e-02-isolation incl. T1 adversarial suite + T2/T3 ratchets;
  guarded podman steps SKIP — no podman on this host, recorded as in gate
  02/06; open issue carried to the podman-capable CI job).
- No amend, no force-push, shared history untouched.

**GATE 02/07: PASS** — merge SHA `f973675`, tag `sprint/02-container-runtime`.
