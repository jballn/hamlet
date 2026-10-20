//! Rootless podman invocation layer (SPEC §4; ADR-0010).
//!
//! Async wrapper over the `podman` CLI through tokio process pipes — no
//! libpod bindings, no Docker, no privileged mode, never a shell. Arguments
//! are built as `Vec<String>` from typed sources only (ADR-0010 cl. 1–2):
//! volume flags exclusively from [`MountSet::to_podman_args`] (the normative
//! SPEC §4 table), env flags from validated [`Blueprint::env`], image refs
//! from [`BlueprintMeta::base_image`], names from [`SafeName`] or numeric
//! UIDs. Raw paths/specs never reach argv.
//!
//! # Isolation policy (fixed by ADR-0010 cl. 3–4, as amended by ADR-0011)
//!
//! Every `run_detached` argv carries:
//! - `--user <uid>:<uid>` over podman's DEFAULT rootless private userns
//!   (host user → 0; subuid[i] → i+1) so the container user *is* the
//!   charter UID slot (Mayor: 1000) with the same mapping in every
//!   container — the prerequisite for ownership-based cross-UID isolation
//!   on shared mounts (ADR-0011; no `--userns` flag is ever passed);
//! - `--security-opt=no-new-privileges` and `--cap-drop=ALL`;
//! - resource-limit flags (`--memory`, `--cpus`, `--pids-limit`) sourced
//!   from the `config.json` `resources` placeholders (binding policy ADR is
//!   Sprint 07; defaults conservative);
//! - and never `--privileged`, `--userns=host`, `--pid=host`,
//!   `--network=host`, socket mounts, or `--authfile`.
//!
//! # Failure policy (ADR-0010 cl. 5)
//!
//! Every failure path produces a [`PodmanError`] carrying podman's stderr
//! **verbatim** and the full argv — with `-e KEY=VALUE` env values masked
//! ([`redacted_argv`]), because logs never carry env values or secrets
//! (CONVENTIONS §6). [`PodmanError`]'s `Display` renders both.
//!
//! # Test seam
//!
//! The [`Runtime`] trait is the seam: [`FakeRuntime`] (records argv,
//! constructs it through the same pure builders, spawns nothing) powers all
//! non-guarded tests including the argv goldens and the forbidden-flag
//! audit. Real podman lives behind [`PodmanRuntime`] and is exercised only
//! by `#[ignore]`-guarded tests under `HAMLET_PODMAN_TESTS=1`
//! (CONVENTIONS §5; see `tests/podman_guarded.rs`).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::config::Config;
use hamlet_core::mount::{MountError, MountSet, MountSpec};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::{FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};
use hamlet_core::tree::HamletTree;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};

/// Boxed future alias — keeps [`Runtime`] dyn-compatible without an
/// `async_trait` dependency (the stack is locked; CONVENTIONS §2).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Exit-code sentinel reported by [`ExecHandle::wait`] when the process was
/// killed by a signal (Unix `ExitStatus::code()` is `None` in that case).
pub const SIGNAL_EXIT_CODE: i32 = -1;

/// podman's own convention: 125 = the `podman` command itself failed
/// (as opposed to the in-container command's exit code).
const PODMAN_SELF_ERROR_CODE: i32 = 125;

/// Default keep-alive entrypoint for detached containers: a slim process
/// the daemon controls (SPEC §4 — no systemd-class init). Sprint 03 drives
/// pi sessions through `podman exec`, so the container body just waits.
pub const KEEP_ALIVE_COMMAND: [&str; 2] = ["sleep", "infinity"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong during a podman invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodmanErrorKind {
    /// The `podman` binary could not be spawned (missing, not executable);
    /// carries the io error message.
    Spawn(String),
    /// podman exited non-zero.
    NonZeroExit { code: Option<i32> },
    /// podman reported the container does not exist.
    NoSuchContainer { name: String },
    /// The normative mount table could not be rendered (SPEC §4).
    Mount(MountError),
    /// A typed source failed pre-flight validation (name, image, command,
    /// env, resource limits) — nothing was spawned.
    InvalidSpec(String),
}

impl fmt::Display for PodmanErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(msg) => write!(f, "spawn failed: {msg}"),
            Self::NonZeroExit { code } => match code {
                Some(c) => write!(f, "podman exited with code {c}"),
                None => write!(f, "podman was killed by a signal"),
            },
            Self::NoSuchContainer { name } => write!(f, "no such container: {name}"),
            Self::Mount(e) => write!(f, "mount table: {e}"),
            Self::InvalidSpec(msg) => write!(f, "invalid spec: {msg}"),
        }
    }
}

/// Structured podman failure (ADR-0010 cl. 5): kind + the full argv
/// (env values masked — never env/secrets in logs, CONVENTIONS §6) +
/// podman's stderr **verbatim**. `Display` renders all three, so every
/// failure path that logs the error logs the argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanError {
    /// Typed failure reason.
    pub kind: PodmanErrorKind,
    /// The argv that was (or would have been) executed, with `-e` values
    /// masked by [`redacted_argv`]. Empty for pre-flight validation errors.
    pub argv: Vec<String>,
    /// podman's stderr, verbatim (empty for pre-flight/spawn errors).
    pub stderr: String,
}

impl PodmanError {
    /// Pre-flight validation failure — nothing was spawned, no argv exists.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self {
            kind: PodmanErrorKind::InvalidSpec(msg.into()),
            argv: Vec::new(),
            stderr: String::new(),
        }
    }

    /// Mount-table rendering failure; argv is the partially built command.
    pub fn mount(err: MountError, argv: Vec<String>) -> Self {
        Self {
            kind: PodmanErrorKind::Mount(err),
            argv: redacted_argv(&argv),
            stderr: String::new(),
        }
    }

    /// Spawn (io) failure.
    pub fn spawn(io_msg: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            kind: PodmanErrorKind::Spawn(io_msg.into()),
            argv: redacted_argv(&argv),
            stderr: String::new(),
        }
    }

    /// Non-zero exit with captured stderr.
    pub fn exit(code: Option<i32>, argv: Vec<String>, stderr: String) -> Self {
        Self {
            kind: PodmanErrorKind::NonZeroExit { code },
            argv: redacted_argv(&argv),
            stderr,
        }
    }

    /// Container-not-found mapping of a non-zero exit.
    pub fn no_such_container(name: impl Into<String>, argv: Vec<String>, stderr: String) -> Self {
        Self {
            kind: PodmanErrorKind::NoSuchContainer { name: name.into() },
            argv: redacted_argv(&argv),
            stderr,
        }
    }
}

impl fmt::Display for PodmanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "podman: {}: {}", self.kind, argv_display(&self.argv))?;
        if !self.stderr.is_empty() {
            write!(f, "; stderr: {}", self.stderr)?;
        }
        Ok(())
    }
}

impl std::error::Error for PodmanError {}

fn argv_display(argv: &[String]) -> String {
    if argv.is_empty() {
        "(no argv — pre-flight rejection)".to_string()
    } else {
        argv.join(" ")
    }
}

/// Mask env values in an argv for logging: every `-e KEY=VALUE` (and
/// `--env=KEY=VALUE`) pair becomes `-e KEY=<redacted>`. All other args pass
/// through unchanged — the argv skeleton (flags, mounts, image) stays fully
/// auditable while env values never reach logs (ADR-0010 cl. 5;
/// CONVENTIONS §6).
#[must_use]
pub fn redacted_argv(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for arg in argv {
        if redact_next {
            redact_next = false;
            out.push(redact_kv(arg));
            continue;
        }
        match arg.as_str() {
            "-e" | "--env" => {
                redact_next = true;
                out.push(arg.clone());
            }
            _ => {
                if let Some(rest) = arg
                    .strip_prefix("--env=")
                    .or_else(|| arg.strip_prefix("-e").filter(|r| r.contains('=')))
                {
                    out.push(arg[..arg.len() - rest.len()].to_owned() + &redact_kv(rest));
                } else {
                    out.push(arg.clone());
                }
            }
        }
    }
    out
}

fn redact_kv(kv: &str) -> String {
    match kv.split_once('=') {
        Some((k, _)) => format!("{k}=<redacted>"),
        None => "<redacted>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Typed state / output
// ---------------------------------------------------------------------------

/// Container lifecycle state as reported by `podman inspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerState {
    /// Up and running.
    Running,
    /// Stopped (any exit code).
    Exited,
    /// Frozen via cgroup freezer.
    Paused,
    /// Anything podman reports that we don't model (created, restarting, …).
    Unknown,
}

/// Parse `podman inspect --format {{.State.Status}}` stdout.
#[must_use]
pub fn parse_container_state(raw: &str) -> ContainerState {
    match raw.trim() {
        "running" => ContainerState::Running,
        "exited" => ContainerState::Exited,
        "paused" => ContainerState::Paused,
        _ => ContainerState::Unknown,
    }
}

/// Buffered result of a `podman exec` run-to-completion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecOutput {
    /// Exit code of the in-container command (`-1` = killed by signal).
    pub code: i32,
    /// Raw stdout.
    pub stdout: Vec<u8>,
    /// Raw stderr.
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    /// Stdout as UTF-8 (lossy).
    #[must_use]
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as UTF-8 (lossy).
    #[must_use]
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (typed sources only — ADR-0010 cl. 2)
// ---------------------------------------------------------------------------

/// Container names pass the §4 sanitizer: this rejects leading `-`
/// (flag injection like `--all`), separators, control chars, and anything
/// outside the allowlist before the name reaches a positional argv slot.
fn validate_container_name(name: &str) -> Result<(), PodmanError> {
    sanitize_name(name)
        .map_err(|e| PodmanError::invalid(format!("container name {name:?}: {e}")))?;
    Ok(())
}

fn validate_image(image: &str) -> Result<(), PodmanError> {
    if image.is_empty()
        || image.starts_with('-')
        || image.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(PodmanError::invalid(format!("image ref {image:?}")));
    }
    Ok(())
}

fn validate_cmd(cmd: &[String]) -> Result<(), PodmanError> {
    if cmd.is_empty() {
        return Err(PodmanError::invalid("command must not be empty"));
    }
    for arg in cmd {
        if arg.is_empty() || arg.chars().any(char::is_control) {
            return Err(PodmanError::invalid(format!("command arg {arg:?}")));
        }
    }
    if cmd[0].starts_with('-') {
        return Err(PodmanError::invalid(format!(
            "command {:?} must not start with '-' (flag-injection guard)",
            cmd[0]
        )));
    }
    Ok(())
}

/// Environment keys are identifiers (mirrors `hamlet-core` blueprint
/// validation — `[env]` is non-secret by contract); values must be free of
/// control characters. Applied again here because [`RunSpec`] fields are
/// public and may be assembled by hand.
fn validate_env(env: &BTreeMap<String, String>) -> Result<(), PodmanError> {
    for (key, value) in env {
        let key_ok = !key.is_empty()
            && key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !key_ok {
            return Err(PodmanError::invalid(format!(
                "env key {key:?} is not an identifier"
            )));
        }
        if value.chars().any(char::is_control) {
            return Err(PodmanError::invalid(format!(
                "env value for {key:?} contains control characters"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resource limits (config.json placeholders; policy ADR is Sprint 07)
// ---------------------------------------------------------------------------

/// Container resource limits wired into every `podman run`. Values come
/// from the `config.json` `resources` section ([`Config`]); charset-checked
/// again at argv-build time because they land verbatim in argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimits {
    /// `--memory` value (e.g. `512m`).
    pub memory: String,
    /// `--cpus` value (e.g. `1.0`, `50%`).
    pub cpus: String,
    /// `--pids-limit` (1..=65535).
    pub pids_limit: u64,
}

impl From<&Config> for ResourceLimits {
    fn from(config: &Config) -> Self {
        Self {
            memory: config.resources.memory.clone(),
            cpus: config.resources.cpus.clone(),
            pids_limit: config.resources.pids_limit,
        }
    }
}

impl ResourceLimits {
    /// # Errors
    ///
    /// [`PodmanError`] when a value is outside the podman charset/range
    /// (leading `-`, separators, whitespace, `pids_limit` out of 1..=65535).
    pub fn validate(&self) -> Result<(), PodmanError> {
        let mem_ok = !self.memory.is_empty()
            && self.memory.len() <= 16
            && self
                .memory
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
            && self.memory.chars().all(|c| c.is_ascii_alphanumeric());
        if !mem_ok {
            return Err(PodmanError::invalid(format!(
                "resources.memory {:?} is not a podman byte value",
                self.memory
            )));
        }
        let cpus_ok = !self.cpus.is_empty()
            && self.cpus.len() <= 8
            && self.cpus.chars().next().is_some_and(|c| c.is_ascii_digit())
            && self
                .cpus
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == '%');
        if !cpus_ok {
            return Err(PodmanError::invalid(format!(
                "resources.cpus {:?} is not a podman cpu value",
                self.cpus
            )));
        }
        if self.pids_limit == 0 || self.pids_limit > 65535 {
            return Err(PodmanError::invalid(format!(
                "resources.pids_limit {} outside 1..=65535",
                self.pids_limit
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RunSpec
// ---------------------------------------------------------------------------

/// Everything needed to render one `podman run --detach` argv. Built from
/// typed sources only ([`RunSpec::for_agent`] / [`RunSpec::for_mayor`]);
/// hand-assembly is possible but re-validated at argv build time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// Container name (`--name`); a sanitized identity.
    pub name: SafeName,
    /// Container hostname (`--hostname`): `parcel<uid>` / `manor`.
    pub hostname: SafeName,
    /// Charter UID slot — the container *user* (ADR-0010 cl. 3).
    pub uid: u32,
    /// Image reference: the blueprint's `[meta].base_image` (ADR-0010 cl. 2)
    /// or — for parcels — the daemon-derived image built from it
    /// ([`RunSpec::with_image`], ADR-0012 cl. 2).
    pub image: String,
    /// The normative SPEC §4 mount set (closed enum; no raw paths).
    pub mounts: MountSet,
    /// Non-secret env defaults from the blueprint's `[env]`.
    pub env: BTreeMap<String, String>,
    /// Resource-limit flags from `config.json` placeholders.
    pub resources: ResourceLimits,
    /// `--workdir`: the SPEC §4 home mount for this UID. Always
    /// `/home/<uid>` — validated against the mount table, never free-form
    /// (a workdir outside the home would put sessions in a directory the
    /// slot does not own).
    pub workdir: String,
    /// Detached body command; defaults to [`KEEP_ALIVE_COMMAND`].
    pub command: Vec<String>,
}

impl RunSpec {
    /// Agent container spec: mount set from [`MountSet::for_agent`] (hard
    /// error outside UID 1001–2000), image/env from the blueprint, resource
    /// flags from config.
    ///
    /// # Errors
    ///
    /// [`PodmanError`] when `uid` is not a valid subagent slot or the
    /// hostname cannot be derived.
    pub fn for_agent(
        identity: &SafeName,
        uid: u32,
        commission: u64,
        blueprint: &Blueprint,
        config: &Config,
    ) -> Result<Self, PodmanError> {
        let mounts =
            MountSet::for_agent(uid, commission).map_err(|e| PodmanError::mount(e, Vec::new()))?;
        Ok(Self::assemble(
            identity,
            uid,
            blueprint,
            config,
            mounts,
            hostname_for_uid(uid)?,
        ))
    }

    /// Mayor container spec: UID 1000, [`MountSet::for_mayor`] (manor home,
    /// no commission git bind).
    ///
    /// # Errors
    ///
    /// [`PodmanError`] on hostname derivation failure (infallible in
    /// practice; kept typed for symmetry).
    pub fn for_mayor(
        identity: &SafeName,
        blueprint: &Blueprint,
        config: &Config,
    ) -> Result<Self, PodmanError> {
        Ok(Self::assemble(
            identity,
            MAYOR_UID,
            blueprint,
            config,
            MountSet::for_mayor(),
            hostname_for_uid(MAYOR_UID)?,
        ))
    }

    /// Override the detached body command (default: keep-alive). Sprint 04
    /// bootstrap may replace it; the command is re-validated at argv build.
    #[must_use]
    pub fn with_command(mut self, command: Vec<String>) -> Self {
        self.command = command;
        self
    }

    /// Override the image reference with the daemon-derived parcel image
    /// (ADR-0012 cl. 2: `base_image` is the PARENT; the apk tools and the
    /// bootstrap floor live in a derived local layer). Re-validated at argv
    /// build like every hand-settable field.
    #[must_use]
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    fn assemble(
        identity: &SafeName,
        uid: u32,
        blueprint: &Blueprint,
        config: &Config,
        mounts: MountSet,
        hostname: SafeName,
    ) -> Self {
        let workdir = home_container_dir(uid);
        let mut env = blueprint.env().clone();
        // `--user <uid>` with no /etc/passwd entry leaves HOME unset (or
        // "/"), which breaks `~`, login-shell profile sourcing and every
        // installer that writes to $HOME. The daemon sets it from the SPEC
        // §4 home mount — and bootstrap refuses a blueprint that declares
        // HOME itself (ADR-0014 cl. 2).
        env.insert("HOME".to_owned(), workdir.clone());
        Self {
            name: identity.clone(),
            hostname,
            uid,
            image: blueprint.meta().base_image().to_owned(),
            mounts,
            env,
            resources: ResourceLimits::from(config),
            workdir,
            command: KEEP_ALIVE_COMMAND.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Render the full `podman run` argv — **pure**, deterministic order,
    /// nothing executed (ADR-0010 cl. 1–2). Element 0 is `podman_bin`.
    ///
    /// # Errors
    ///
    /// [`PodmanError`] on any validation failure (UID outside the slot
    /// policy, image/command/env/resource charset) or mount-render failure
    /// ([`MountError::HostPathColon`]).
    pub fn to_run_argv(
        &self,
        tree: &HamletTree,
        podman_bin: &Path,
    ) -> Result<Vec<String>, PodmanError> {
        self.validate()?;
        let uid = self.uid;
        let mut argv: Vec<String> = vec![
            podman_bin.to_string_lossy().into_owned(),
            "run".to_owned(),
            "-d".to_owned(),
            "--name".to_owned(),
            self.name.to_string(),
            "--hostname".to_owned(),
            self.hostname.to_string(),
            // ADR-0011 (amending ADR-0010 cl. 3): DEFAULT rootless private
            // userns (subuid mapping, identical in every container of this
            // host user) + `--user` — the container user IS the slot uid.
            // No `--userns` flag: keep-id would collapse cross-UID ownership
            // on shared mounts; `host` is forbidden (ADR-0010 cl. 4).
            "--user".to_owned(),
            format!("{uid}:{uid}"),
            "--workdir".to_owned(),
            self.workdir.clone(),
            // ADR-0010 cl. 4: required hardening.
            "--security-opt=no-new-privileges".to_owned(),
            "--cap-drop=ALL".to_owned(),
            // Resource limits — config.json placeholders (policy: Sprint 07).
            "--memory".to_owned(),
            self.resources.memory.clone(),
            "--cpus".to_owned(),
            self.resources.cpus.clone(),
            "--pids-limit".to_owned(),
            self.resources.pids_limit.to_string(),
        ];
        // Volumes: exclusively from the typed SPEC §4 table.
        argv.extend(
            self.mounts
                .to_podman_args(tree)
                .map_err(|e| PodmanError::mount(e, argv.clone()))?,
        );
        // Env: blueprint [env], sorted (BTreeMap), values masked in logs.
        for (key, value) in &self.env {
            argv.push("-e".to_owned());
            argv.push(format!("{key}={value}"));
        }
        argv.push(self.image.clone());
        argv.extend(self.command.iter().cloned());
        Ok(argv)
    }

    /// Pre-flight validation of every hand-settable field.
    ///
    /// # Errors
    ///
    /// [`PodmanError`] on the first violation.
    pub fn validate(&self) -> Result<(), PodmanError> {
        if self.uid != MAYOR_UID && !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&self.uid) {
            return Err(PodmanError::invalid(format!(
                "uid {} is neither the Mayor ({MAYOR_UID}) nor a subagent slot \
                 ({FIRST_AGENT_UID}..={LAST_AGENT_UID})",
                self.uid
            )));
        }
        validate_image(&self.image)?;
        validate_cmd(&self.command)?;
        validate_env(&self.env)?;
        let home = home_container_dir(self.uid);
        if self.workdir != home {
            return Err(PodmanError::invalid(format!(
                "workdir {:?} must be this UID's SPEC §4 home mount {home:?}",
                self.workdir
            )));
        }
        match self.env.get("HOME") {
            Some(value) if value == &home => {}
            Some(value) => {
                return Err(PodmanError::invalid(format!(
                    "env HOME must be the SPEC §4 home mount {home:?}, not {value:?}"
                )))
            }
            None => {
                return Err(PodmanError::invalid(
                    "env HOME is mandatory (numeric --user has no passwd entry)",
                ))
            }
        }
        self.resources.validate()
    }
}

/// The SPEC §4 home mount for a UID, without the trailing slash: the single
/// source of `HOME`/`--workdir` (derived from the typed mount table, never a
/// format! literal — CONVENTIONS §2 typed paths).
#[must_use]
pub fn home_container_dir(uid: u32) -> String {
    let spec = if uid == MAYOR_UID {
        MountSpec::ManorHome
    } else {
        MountSpec::AgentHome { uid }
    };
    spec.container_path().trim_end_matches('/').to_owned()
}

fn hostname_for_uid(uid: u32) -> Result<SafeName, PodmanError> {
    let raw = if uid == MAYOR_UID {
        "manor".to_owned()
    } else {
        format!("parcel{uid}")
    };
    sanitize_name(&raw).map_err(|e| PodmanError::invalid(format!("hostname {raw:?}: {e}")))
}

// ---------------------------------------------------------------------------
// Pure argv builders for the lifecycle verbs
// ---------------------------------------------------------------------------

/// `podman exec` argv (`-i` only when stdin is piped). Pure.
///
/// # Errors
///
/// [`PodmanError`] when the container name fails the §4 sanitizer or the
/// command is empty/control-char/leading-dash.
pub fn exec_argv(
    podman_bin: &Path,
    container: &str,
    cmd: &[String],
    pipe_stdin: bool,
) -> Result<Vec<String>, PodmanError> {
    validate_container_name(container)?;
    validate_cmd(cmd)?;
    let mut argv = vec![podman_bin.to_string_lossy().into_owned(), "exec".to_owned()];
    if pipe_stdin {
        argv.push("-i".to_owned());
    }
    argv.push(container.to_owned());
    argv.extend(cmd.iter().cloned());
    Ok(argv)
}

/// `podman exec -i --user <uid>:<uid> --workdir /home/<uid> [-e K=V]…
/// <container> <cmd…>` argv — the pi session spawn form (ADR-0015 cl. 1).
/// Pure; env renders in sorted order. The workdir is always the slot's
/// SPEC §4 home mount (typed source: [`home_container_dir`], never a
/// format! literal).
///
/// # Errors
///
/// [`PodmanError`] when the container name fails the §4 sanitizer, the uid
/// is outside the slot policy, any env key/value fails validation, or the
/// command is empty/control-char/leading-dash.
pub fn session_exec_argv(
    podman_bin: &Path,
    spec: &SessionExecSpec,
) -> Result<Vec<String>, PodmanError> {
    validate_container_name(&spec.container)?;
    validate_session_uid(spec.uid)?;
    validate_env(&spec.env)?;
    validate_cmd(&spec.cmd)?;
    let uid = spec.uid;
    let mut argv = vec![
        podman_bin.to_string_lossy().into_owned(),
        "exec".to_owned(),
        "-i".to_owned(),
        "--user".to_owned(),
        format!("{uid}:{uid}"),
        "--workdir".to_owned(),
        home_container_dir(uid),
    ];
    for (key, value) in &spec.env {
        argv.push("-e".to_owned());
        argv.push(format!("{key}={value}"));
    }
    argv.push(spec.container.clone());
    argv.extend(spec.cmd.iter().cloned());
    Ok(argv)
}

fn validate_session_uid(uid: u32) -> Result<(), PodmanError> {
    if uid != MAYOR_UID && !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
        return Err(PodmanError::invalid(format!(
            "session uid {uid} is neither the Mayor ({MAYOR_UID}) nor a subagent slot \
             ({FIRST_AGENT_UID}..={LAST_AGENT_UID})"
        )));
    }
    Ok(())
}

/// `podman stop --time <grace>` argv. Pure.
///
/// # Errors
///
/// [`PodmanError`] when the container name fails the §4 sanitizer.
pub fn stop_argv(
    podman_bin: &Path,
    container: &str,
    grace: Duration,
) -> Result<Vec<String>, PodmanError> {
    validate_container_name(container)?;
    Ok(vec![
        podman_bin.to_string_lossy().into_owned(),
        "stop".to_owned(),
        "--time".to_owned(),
        grace.as_secs().to_string(),
        container.to_owned(),
    ])
}

/// `podman kill` argv (SIGKILL). Pure.
///
/// # Errors
///
/// [`PodmanError`] when the container name fails the §4 sanitizer.
pub fn kill_argv(podman_bin: &Path, container: &str) -> Result<Vec<String>, PodmanError> {
    validate_container_name(container)?;
    Ok(vec![
        podman_bin.to_string_lossy().into_owned(),
        "kill".to_owned(),
        container.to_owned(),
    ])
}

/// `podman inspect --format {{.State.Status}}` argv. Pure.
///
/// # Errors
///
/// [`PodmanError`] when the container name fails the §4 sanitizer.
pub fn state_argv(podman_bin: &Path, container: &str) -> Result<Vec<String>, PodmanError> {
    validate_container_name(container)?;
    Ok(vec![
        podman_bin.to_string_lossy().into_owned(),
        "inspect".to_owned(),
        "--format".to_owned(),
        "{{.State.Status}}".to_owned(),
        container.to_owned(),
    ])
}

/// `podman rm [-f] <container>` argv (rollback / hibernation cleanup).
/// Pure.
///
/// # Errors
///
/// [`PodmanError`] when the container name fails the §4 sanitizer.
pub fn remove_argv(
    podman_bin: &Path,
    container: &str,
    force: bool,
) -> Result<Vec<String>, PodmanError> {
    validate_container_name(container)?;
    let mut argv = vec![podman_bin.to_string_lossy().into_owned(), "rm".to_owned()];
    if force {
        argv.push("--force".to_owned());
    }
    argv.push(container.to_owned());
    Ok(argv)
}

/// `podman unshare chown <uid>:<gid> -- <paths…>` argv (ADR-0011 cl. 2):
/// per-UID host ownership through the daemon's own userns — the same subuid
/// mapping containers see, no real root. Pure.
///
/// Paths come from [`HamletTree`] accessors in practice; each is still
/// validated (absolute, UTF-8, no control chars, never a leading `-`) and
/// placed after a `--` separator so a hostile path can never become a
/// `chown` flag.
///
/// # Errors
///
/// [`PodmanError`] on an empty path list or any invalid path.
pub fn unshare_chown_argv(
    podman_bin: &Path,
    uid: u32,
    gid: u32,
    paths: &[PathBuf],
) -> Result<Vec<String>, PodmanError> {
    unshare_chown_argv_inner(podman_bin, uid, gid, paths, false)
}

/// `podman unshare chown -RP <uid>:<gid> -- <paths…>` argv (ADR-0011 cl. 2,
/// ADR-0034 cl. 1 + cl. 5): the same brace, extended to everything already
/// inside the paths.
///
/// **`-R` always carries an explicit `-P`** (ADR-0034 cl. 5, QA04-29). The
/// manor home is agent-writable by design, so the brace walks a tree the
/// adversary can plant symlinks in. GNU `chown`'s traversal default is `-P`
/// (never dereference) — safe, but *undocumented in this codebase and unpinned
/// by any test*: a `-L`/`-H` "improvement", a BusyBox `chown`, or a different
/// host's coreutils would silently turn this into a **privileged chown of an
/// attacker-chosen tree** (measured live: `-RL` on the same fixture moves the
/// symlink *target* to the Mayor's subuid, rc=0, no warning). Naming the
/// policy in the argv makes the safe reading the *declared* one and the unsafe
/// one a loud `invalid option` instead of a silent escalation.
///
/// # Errors
///
/// [`PodmanError`] on an empty path list or any invalid path.
pub fn unshare_chown_recursive_argv(
    podman_bin: &Path,
    uid: u32,
    gid: u32,
    paths: &[PathBuf],
) -> Result<Vec<String>, PodmanError> {
    unshare_chown_argv_inner(podman_bin, uid, gid, paths, true)
}

fn unshare_chown_argv_inner(
    podman_bin: &Path,
    uid: u32,
    gid: u32,
    paths: &[PathBuf],
    recursive: bool,
) -> Result<Vec<String>, PodmanError> {
    if paths.is_empty() {
        return Err(PodmanError::invalid("unshare chown: no paths"));
    }
    let mut argv = vec![
        podman_bin.to_string_lossy().into_owned(),
        "unshare".to_owned(),
    ];
    argv.push("chown".to_owned());
    if recursive {
        // Fixed operand, in this order: recurse, never dereference.
        argv.push("-RP".to_owned());
    }
    argv.push(format!("{uid}:{gid}"));
    argv.push("--".to_owned());
    for path in paths {
        validate_host_path(path)?;
        argv.push(path.to_string_lossy().into_owned());
    }
    Ok(argv)
}

fn validate_host_path(path: &Path) -> Result<(), PodmanError> {
    let text = path.to_str().ok_or_else(|| {
        PodmanError::invalid(format!("host path {} is not valid UTF-8", path.display()))
    })?;
    if text.is_empty()
        || !text.starts_with('/')
        || text.starts_with('-')
        || text.chars().any(char::is_control)
    {
        return Err(PodmanError::invalid(format!(
            "host path {text:?} must be absolute and free of control characters"
        )));
    }
    Ok(())
}

/// `podman unshare <cmd…>` argv (ADR-0011 cl. 4, ADR-0021 cl. 2): run a
/// HOST binary inside the daemon's own userns — root-equivalent over the
/// subuid-owned village tree, ordinary user on the bare host. Never a
/// shell; `cmd[0]` is the binary and is validated like every `exec`
/// command (no leading-dash flag injection, no control chars). Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid command payload.
pub fn unshare_exec_argv(podman_bin: &Path, cmd: &[String]) -> Result<Vec<String>, PodmanError> {
    validate_cmd(cmd)?;
    let mut argv = vec![
        podman_bin.to_string_lossy().into_owned(),
        "unshare".to_owned(),
    ];
    argv.extend(cmd.iter().cloned());
    Ok(argv)
}

/// `getfacl -p -n -- <path>` payload for [`unshare_exec_argv`]
/// (ADR-0021 cl. 2): numeric ids (the in-userns slot UIDs), full path
/// comment. Pure.
///
/// **No `-c`/`--omit-header`, deliberately** (QA04-12): that flag suppresses
/// exactly the `# owner:` / `# group:` comment lines the audit's first two
/// rules are derived from — "owner is a charter slot" and the group check —
/// so with `-c` every real listing failed to parse as
/// [`MissingOwner`](crate::freight::FreightError::MissingOwner). The parser
/// ignores `# file:`/`# flags:`/blank lines, so the un-omitted header is
/// harmless noise and the *owner* is mandatory state. Verified against GNU
/// acl 2.4.0, the host tool ADR-0021 cl. 2 names as a prerequisite.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn getfacl_cmd(path: &Path) -> Result<Vec<String>, PodmanError> {
    validate_host_path(path)?;
    Ok(vec![
        "getfacl".to_owned(),
        "-p".to_owned(),
        "-n".to_owned(),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ])
}

/// `stat -c %F -- <path>` payload for [`unshare_exec_argv`] (ADR-0032
/// cl. 1 / QA04-13): the userns-side *kind* probe the freight promotion-read
/// uses when the daemon's host user may not `lstat` a 0700 subuid-owned
/// sandbox. `-c %F` prints the human-readable file type; no `-L`, so the
/// probe keeps `lstat` semantics and a symlinked target is still reported as
/// `symbolic link`. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn stat_cmd(path: &Path) -> Result<Vec<String>, PodmanError> {
    validate_host_path(path)?;
    Ok(vec![
        "stat".to_owned(),
        "-c".to_owned(),
        "%F".to_owned(),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ])
}

/// `setfacl -m <mod>… -- <path>` payload for [`unshare_exec_argv`]
/// (ADR-0021 cl. 2). Each mod must be a `u:<uid>:<rwx>` grant token as
/// rendered by [`hamlet_core::freight::AclGrant::token`] — anything else
/// is rejected (charset-safe by construction, no injection surface).
/// Pure.
///
/// # Errors
///
/// [`PodmanError`] on an empty mod list, a malformed mod token, or an
/// invalid host path.
pub fn setfacl_cmd(mods: &[String], path: &Path) -> Result<Vec<String>, PodmanError> {
    if mods.is_empty() {
        return Err(PodmanError::invalid("setfacl: no -m modifications"));
    }
    let mut cmd = vec!["setfacl".to_owned()];
    for m in mods {
        validate_acl_mod(m)?;
        cmd.push("-m".to_owned());
        cmd.push(m.clone());
    }
    cmd.push("--".to_owned());
    validate_host_path(path)?;
    cmd.push(path.to_string_lossy().into_owned());
    Ok(cmd)
}

/// `u:<uid>:<perms>` grant-token validation: exactly the shape
/// [`hamlet_core::freight::AclGrant::token`] renders — digits-only uid,
/// perms strictly `[r-][w-][x-]`.
fn validate_acl_mod(m: &str) -> Result<(), PodmanError> {
    let invalid = || {
        PodmanError::invalid(format!(
            "setfacl mod {m:?} is not a u:<uid>:[r-][w-][x-] token"
        ))
    };
    let parts: Vec<&str> = m.split(':').collect();
    if parts.len() != 3 || parts[0] != "u" {
        return Err(invalid());
    }
    if parts[1].is_empty() || !parts[1].chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    let perms: Vec<char> = parts[2].chars().collect();
    let ok = perms.len() == 3
        && (perms[0] == 'r' || perms[0] == '-')
        && (perms[1] == 'w' || perms[1] == '-')
        && (perms[2] == 'x' || perms[2] == '-');
    if !ok {
        return Err(invalid());
    }
    Ok(())
}

/// `cat -- <path>` payload for [`unshare_exec_argv`] — the promotion-read
/// primitive (ADR-0021 cl. 6): daemon read access to any freight artifact
/// regardless of agent-side ACLs, without relaxing them. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn cat_cmd(path: &Path) -> Result<Vec<String>, PodmanError> {
    validate_host_path(path)?;
    Ok(vec![
        "cat".to_owned(),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ])
}

/// `mktemp -- <dir>/.<prefix>XXXXXX` payload for [`unshare_exec_argv`] —
/// step one of [`unshare_write_atomic`].
///
/// `mktemp` creates its output with **`O_CREAT|O_EXCL`** and a name nobody
/// can predict from outside the call, which is exactly the ADR-0007
/// create-new property the daemon's own [`create_new`] temp walk buys in
/// user space. That is why the write seam goes through it rather than
/// guessing a name: the Mayor controls `manor/home/decrees/` and can
/// pre-plant any *predictable* name (QA05-04), but cannot pre-plant six
/// unpredictable ones. The resulting file is mode 0600 and owned by the
/// userns root. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path or a non-sanitizable prefix.
pub fn mktemp_staged_cmd(dir: &Path, prefix: &str) -> Result<Vec<String>, PodmanError> {
    validate_host_path(dir)?;
    let prefix = sanitize_name(prefix)
        .map_err(|e| PodmanError::invalid(format!("mktemp prefix {prefix:?}: {e}")))?;
    Ok(vec![
        "mktemp".to_owned(),
        "--".to_owned(),
        format!("{}/.{prefix}XXXXXX", dir.to_string_lossy()),
    ])
}

/// `cp -- <src> <dest>` payload for [`unshare_exec_argv`] — step two of
/// [`unshare_write_atomic`]: copy the daemon-staged bytes onto the temp
/// file `mktemp` just created exclusively. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn cp_staged_cmd(src: &Path, dest: &Path) -> Result<Vec<String>, PodmanError> {
    validate_host_path(src)?;
    validate_host_path(dest)?;
    Ok(vec![
        "cp".to_owned(),
        "--".to_owned(),
        src.to_string_lossy().into_owned(),
        dest.to_string_lossy().into_owned(),
    ])
}

/// `chmod 0600 -- <path>` payload for [`unshare_exec_argv`] — step three of
/// [`unshare_write_atomic`]. `0600` is correct only *after* the chown: the
/// file's owner becomes the slot UID, which is exactly who may read it.
/// Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn chmod_staged_cmd(mode: &str, path: &Path) -> Result<Vec<String>, PodmanError> {
    if mode.len() != 4 || !mode.chars().all(|c| c.is_ascii_digit()) {
        return Err(PodmanError::invalid(format!("chmod mode {mode:?}")));
    }
    validate_host_path(path)?;
    Ok(vec![
        "chmod".to_owned(),
        mode.to_owned(),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ])
}

/// `chown <uid>:<gid> -- <path>` payload for [`unshare_exec_argv`] — step
/// four of [`unshare_write_atomic`]: hand the file to the subuid whose home
/// it lives in (ADR-0011 cl. 2), so the daemon still owns no part of it
/// afterwards. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn chown_one_cmd(uid: u32, gid: u32, path: &Path) -> Result<Vec<String>, PodmanError> {
    validate_host_path(path)?;
    Ok(vec![
        "chown".to_owned(),
        format!("{uid}:{gid}"),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ])
}

/// `mv -- <src> <dest>` payload for [`unshare_exec_argv`] — the final,
/// atomic step of [`unshare_write_atomic`]: rename inside the same
/// directory, so a reader (the Mayor) never observes a partial ack. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid host path.
pub fn mv_staged_cmd(src: &Path, dest: &Path) -> Result<Vec<String>, PodmanError> {
    validate_host_path(src)?;
    validate_host_path(dest)?;
    Ok(vec![
        "mv".to_owned(),
        "--".to_owned(),
        src.to_string_lossy().into_owned(),
        dest.to_string_lossy().into_owned(),
    ])
}

/// `podman image exists` argv (pull idempotency probe). Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid image ref.
pub fn image_exists_argv(podman_bin: &Path, image: &str) -> Result<Vec<String>, PodmanError> {
    validate_image(image)?;
    Ok(vec![
        podman_bin.to_string_lossy().into_owned(),
        "image".to_owned(),
        "exists".to_owned(),
        image.to_owned(),
    ])
}

/// `podman pull` argv. Pure.
///
/// # Errors
///
/// [`PodmanError`] on an invalid image ref.
pub fn pull_argv(podman_bin: &Path, image: &str) -> Result<Vec<String>, PodmanError> {
    validate_image(image)?;
    Ok(vec![
        podman_bin.to_string_lossy().into_owned(),
        "pull".to_owned(),
        image.to_owned(),
    ])
}

/// `podman build --tag <tag> --file <containerfile> <context>` argv
/// (ADR-0012 cl. 2: derived parcel images). Pure.
///
/// Both paths must be absolute, UTF-8, control-char-free and never
/// leading-dash. The daemon only ever passes a directory it created with
/// mode 0700 under `std::env::temp_dir()` — **never** a path inside the
/// village tree that agent UIDs can write (`market/pkgcache/` is 01777:
/// building from there would hand any agent the image definition, i.e. the
/// container's root filesystem).
///
/// # Errors
///
/// [`PodmanError`] on an invalid image tag or host path.
pub fn build_argv(
    podman_bin: &Path,
    tag: &str,
    containerfile: &Path,
    context: &Path,
) -> Result<Vec<String>, PodmanError> {
    validate_image(tag)?;
    validate_host_path(containerfile)?;
    validate_host_path(context)?;
    Ok(vec![
        podman_bin.to_string_lossy().into_owned(),
        "build".to_owned(),
        "--tag".to_owned(),
        tag.to_owned(),
        "--file".to_owned(),
        containerfile.to_string_lossy().into_owned(),
        context.to_string_lossy().into_owned(),
    ])
}

// ---------------------------------------------------------------------------
// Runtime trait (the seam)
// ---------------------------------------------------------------------------

/// The three stdio pipes of a live session, detached from the handle so a
/// supervisor can read stdout, write stdin, and kill/wait **concurrently**
/// (borrowing accessors cannot be split across tasks — Sprint 03 lesson).
pub struct ExecPipes {
    /// The session's stdin (write side).
    pub stdin: Box<dyn AsyncWrite + Unpin + Send>,
    /// The session's stdout (read side).
    pub stdout: Box<dyn AsyncRead + Unpin + Send>,
    /// The session's stderr (read side).
    pub stderr: Box<dyn AsyncRead + Unpin + Send>,
}

/// A live `podman exec` session with piped stdio — the substrate for pi
/// agent sessions (SPEC §6: JSON-RPC over stdin/stdout, sequential mail
/// delivery, out-of-band interrupts). Implementors: the real child-process
/// handle and [`FakeRuntime`]'s in-memory handle.
pub trait ExecHandle: Send {
    /// Detach the stdio pipes. Returns [`None`] when already taken (pipes
    /// are detached exactly once, right after spawn).
    fn take_pipes(&mut self) -> Option<ExecPipes>;
    /// Hard kill (SIGKILL semantics): the process dies promptly and
    /// [`ExecHandle::wait`] resolves to [`SIGNAL_EXIT_CODE`].
    fn kill(&mut self) -> Result<(), PodmanError>;
    /// Wait for exit; resolves to the exit code ([`SIGNAL_EXIT_CODE`] when
    /// killed by a signal).
    fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<i32, PodmanError>>;
}

/// Spawn a local child process with piped stdio and `kill_on_drop` as an
/// [`ExecHandle`]. Shared substrate of [`Runtime::spawn_exec`],
/// [`Runtime::spawn_session`], and the FakeAgent test spawner
/// ([`spawn_child_exec`] never runs a shell — ADR-0010 cl. 1).
///
/// `env` entries are *added to the child's environment*; only the
/// host-process test spawner passes any (the podman paths carry env as
/// `-e` argv flags instead, ADR-0015 cl. 1). `argv_label` is the full argv
/// used for error reporting — it is passed through [`redacted_argv`] before
/// it can reach any [`PodmanError`], so `-e K=V` values never leak
/// (CONVENTIONS §6).
///
/// # Errors
///
/// [`PodmanError::kind`] `Spawn` when the process cannot be started.
pub fn spawn_child_exec(
    program: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    argv_label: Vec<String>,
) -> Result<Box<dyn ExecHandle>, PodmanError> {
    let argv = redacted_argv(&argv_label);
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| PodmanError::spawn(e.to_string(), argv.clone()))?;
    // piped() above guarantees these are Some.
    let pipes = ExecPipes {
        stdin: Box::new(
            child
                .stdin
                .take()
                .ok_or_else(|| PodmanError::spawn("exec stdin not piped", argv.clone()))?,
        ),
        stdout: Box::new(
            child
                .stdout
                .take()
                .ok_or_else(|| PodmanError::spawn("exec stdout not piped", argv.clone()))?,
        ),
        stderr: Box::new(
            child
                .stderr
                .take()
                .ok_or_else(|| PodmanError::spawn("exec stderr not piped", argv.clone()))?,
        ),
    };
    Ok(Box::new(ChildExecHandle {
        child,
        pipes: Some(pipes),
        argv,
    }))
}

/// Everything needed to render one pi-session spawn argv (ADR-0015 cl. 1):
/// `podman exec -i --user <uid>:<uid> --workdir /home/<uid> [-e K=V]…
/// <container> <cmd…>`. Built from typed sources; re-validated at argv
/// build time like every other spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExecSpec {
    /// Target container name (sanitizer-checked).
    pub container: String,
    /// Charter UID slot — the exec *user* (ADR-0010 cl. 3, ADR-0011).
    pub uid: u32,
    /// `-e` env: structural parcel env + secret injection + pi hygiene
    /// vars ([`redacted_argv`] masks the values in every log path).
    /// Sorted rendering: `BTreeMap` iteration (CONVENTIONS §3 r1).
    pub env: BTreeMap<String, String>,
    /// In-container command (`pi --mode rpc …` in production; validated by
    /// [`validate_cmd`] — no leading-dash flag injection).
    pub cmd: Vec<String>,
}

/// The podman seam (CONVENTIONS §9): [`FakeRuntime`] records argv and
/// spawns nothing; [`PodmanRuntime`] executes for real.
pub trait Runtime: Send + Sync {
    /// Start a detached container; resolves to podman's container ID.
    fn run_detached<'a>(
        &'a self,
        spec: &'a RunSpec,
        tree: &'a HamletTree,
    ) -> BoxFuture<'a, Result<String, PodmanError>>;

    /// Run a command to completion with piped stdio; `stdin` (if any) is
    /// written then closed. Non-zero in-container exit codes are returned
    /// in [`ExecOutput`], not as errors; podman-level failures (exit 125,
    /// missing container) are errors.
    fn exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>>;

    /// Spawn an interactive `podman exec -i` session (piped stdio, live
    /// handle) — consumed by Sprint 03 supervision.
    fn spawn_exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>>;

    /// Spawn a pi session exec (ADR-0015 cl. 1): interactive, as the slot
    /// uid, in the slot home, with `-e` env injection (secrets ride here
    /// and nowhere else — SPEC §11.4) and piped stdio for the JSONL
    /// protocol.
    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>>;

    /// Graceful stop: SIGTERM, then SIGKILL after `grace`.
    fn stop<'a>(
        &'a self,
        container: &'a str,
        grace: Duration,
    ) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// Immediate SIGKILL.
    fn kill<'a>(&'a self, container: &'a str) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// Typed container state.
    fn state<'a>(
        &'a self,
        container: &'a str,
    ) -> BoxFuture<'a, Result<ContainerState, PodmanError>>;

    /// Idempotent image pull: probes `podman image exists` first, then pulls
    /// with bounded retries (pkgcache-friendly: flaky networks must not fail
    /// provisioning on the first hiccup).
    fn pull<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// Local-store probe (`podman image exists`) without any pull — the
    /// build-vs-reuse decision for derived parcel images (ADR-0012 cl. 2),
    /// which must never be pulled from a registry.
    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, PodmanError>>;

    /// `podman build` a derived image from a daemon-private Containerfile
    /// (ADR-0012 cl. 2). Only ever called when [`Runtime::image_exists`]
    /// reports the tag missing, so a village builds each blueprint image
    /// exactly once.
    fn build<'a>(
        &'a self,
        tag: &'a str,
        containerfile: &'a Path,
        context: &'a Path,
    ) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// Remove a container (`podman rm [--force]`) — provisioning rollback
    /// and hibernation cleanup; no orphan containers may survive a failed
    /// flow (Sprint 02 step 03).
    fn remove<'a>(
        &'a self,
        container: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// `podman unshare chown <uid>:<gid> -- <paths…>` (ADR-0011 cl. 2):
    /// establishes per-UID host ownership through the daemon's own userns.
    fn unshare_chown<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// `podman unshare chown -R <uid>:<gid> -- <paths…>` (ADR-0034): the
    /// same ownership brace applied to the whole subtree, for a directory
    /// the daemon pre-creates *inside* a slot's private home. A
    /// non-recursive chown hands the top directory to the slot while every
    /// daemon-created child stays daemon-owned — invisible on the host (the
    /// daemon can always write there) and fatal inside the container, where
    /// that child is unwritable by the very slot that owns its parent.
    fn unshare_chown_recursive<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>>;

    /// `podman unshare <cmd…>` (ADR-0011 cl. 4, ADR-0021 cl. 2): run a
    /// HOST binary as the daemon's userns root over the village tree —
    /// the freight ACL audit (`getfacl`), root grants (`setfacl`) and the
    /// Sprint 06 promotion-read (`cat`) path. Payload argv is built by the
    /// typed [`getfacl_cmd`]/[`setfacl_cmd`]/[`cat_cmd`] helpers, never
    /// free-form. Non-zero exit codes are returned in [`ExecOutput`] for
    /// the caller to judge; podman-level failures (exit 125) are errors.
    fn unshare_exec<'a>(
        &'a self,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>>;

    /// Atomically write `bytes` to `dest` (a path inside a subuid-owned
    /// tree) from **inside the daemon's own userns** (ADR-0011 cl. 4,
    /// QA05-02).
    ///
    /// Why this exists: `manor/home/` is mode 0700 owned by subuid 1000, so
    /// under the spec'd dedicated service user the daemon's direct write of
    /// `<decree_id>.ack` is refused by the kernel. Writing through
    /// `podman unshare` is the seam ADR-0011 cl. 4 prescribes — the same one
    /// the freight ACL audit and the promotion read already use.
    ///
    /// Properties the implementation must hold (ADR-0007 + CONVENTIONS §3
    /// r2), because the Mayor owns the destination directory:
    ///
    /// * **create-new** — the staging temp name is minted by `mktemp`
    ///   (`O_CREAT|O_EXCL`, unpredictable), never derived from `dest`;
    /// * **never a shell** (ADR-0010 cl. 1) — five typed argv payloads, no
    ///   interpolation, each validated by `validate_host_path`;
    /// * **atomic** — the final step is a same-directory `mv`, so a reader
    ///   sees either the old bytes or the whole new file;
    /// * **handed over** — the result is `chown`ed to `owner` and left
    ///   0600, so the daemon keeps no writable handle on a file the Mayor
    ///   owns.
    ///
    /// The default implementation refuses: a runtime that cannot write into
    /// the userns must say so rather than silently appearing to.
    fn unshare_write_atomic<'a>(
        &'a self,
        dest: &'a Path,
        bytes: &'a [u8],
        owner: (u32, u32),
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let _ = (bytes, owner);
            Err(PodmanError::invalid(format!(
                "this runtime has no unshare write seam; cannot write {} from inside the \
                 daemon userns (ADR-0011 cl. 4)",
                dest.display()
            )))
        })
    }
}

// ---------------------------------------------------------------------------
// Real runtime
// ---------------------------------------------------------------------------

/// The real podman CLI runtime: tokio [`Command`] with piped stdio, argv
/// `Vec<String>`, never a shell (ADR-0010 cl. 1).
#[derive(Debug, Clone)]
pub struct PodmanRuntime {
    podman_bin: PathBuf,
    pull_attempts: u32,
    pull_retry_delay: Duration,
}

impl PodmanRuntime {
    /// Runtime over an explicit `podman` binary path (PATH-resolved when
    /// relative).
    #[must_use]
    pub fn new(podman_bin: impl Into<PathBuf>) -> Self {
        Self {
            podman_bin: podman_bin.into(),
            pull_attempts: 3,
            pull_retry_delay: Duration::from_secs(2),
        }
    }

    /// Runtime using `config.podman_bin` (SPEC §4: rootless CLI only).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.podman_bin.clone())
    }

    /// Override pull retry policy (guarded tests shorten it).
    #[must_use]
    pub fn with_pull_retry(mut self, attempts: u32, delay: Duration) -> Self {
        self.pull_attempts = attempts.max(1);
        self.pull_retry_delay = delay;
        self
    }

    /// The configured binary path.
    #[must_use]
    pub fn podman_bin(&self) -> &Path {
        &self.podman_bin
    }

    /// Spawn argv[0..] (element 0 is the binary itself in our argv goldens),
    /// optionally piping stdin; resolves to the buffered output regardless
    /// of exit code.
    async fn spawn_argv(
        &self,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<ExecOutput, PodmanError> {
        let mut cmd = Command::new(&self.podman_bin);
        cmd.args(&argv[1..])
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| PodmanError::spawn(e.to_string(), argv.clone()))?;
        if let Some(data) = stdin {
            // Written from a task so a full pipe buffer can't deadlock the
            // reader side; stdin closes when the task drops the handle.
            if let Some(mut handle) = child.stdin.take() {
                tokio::spawn(async move {
                    let _ = handle.write_all(&data).await;
                    let _ = handle.shutdown().await;
                });
            }
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| PodmanError::spawn(e.to_string(), argv.clone()))?;
        Ok(ExecOutput {
            code: out.status.code().unwrap_or(SIGNAL_EXIT_CODE),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// Spawn + require exit 0; maps podman's "no such container" onto
    /// [`PodmanErrorKind::NoSuchContainer`].
    async fn spawn_ok(&self, argv: Vec<String>) -> Result<ExecOutput, PodmanError> {
        let out = self.spawn_argv(argv.clone(), None).await?;
        if out.code != 0 {
            return Err(map_nonzero(out.code, &argv, out.stderr_lossy()));
        }
        Ok(out)
    }
}

fn map_nonzero(code: i32, argv: &[String], stderr: String) -> PodmanError {
    if stderr.to_lowercase().contains("no such container")
        || stderr.to_lowercase().contains("no container with name")
    {
        let name = argv.last().cloned().unwrap_or_default();
        return PodmanError::no_such_container(name, argv.to_vec(), stderr);
    }
    PodmanError::exit(Some(code), argv.to_vec(), stderr)
}

impl Runtime for PodmanRuntime {
    fn run_detached<'a>(
        &'a self,
        spec: &'a RunSpec,
        tree: &'a HamletTree,
    ) -> BoxFuture<'a, Result<String, PodmanError>> {
        Box::pin(async move {
            let argv = spec.to_run_argv(tree, &self.podman_bin)?;
            let out = self.spawn_ok(argv).await?;
            let id = out.stdout_lossy();
            let id = id.trim();
            if id.is_empty() {
                return Err(PodmanError::invalid(
                    "podman run -d produced no container id on stdout",
                ));
            }
            Ok(id.to_owned())
        })
    }

    fn exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>> {
        Box::pin(async move {
            let argv = exec_argv(&self.podman_bin, container, cmd, stdin.is_some())?;
            let out = self
                .spawn_argv(argv.clone(), stdin.map(<[u8]>::to_vec))
                .await?;
            if out.code == PODMAN_SELF_ERROR_CODE
                || out
                    .stderr_lossy()
                    .to_lowercase()
                    .contains("no such container")
            {
                return Err(map_nonzero(out.code, &argv, out.stderr_lossy()));
            }
            Ok(out)
        })
    }

    fn spawn_exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        Box::pin(async move {
            let argv = exec_argv(&self.podman_bin, container, cmd, true)?;
            let args = argv[1..].to_vec();
            spawn_child_exec(&self.podman_bin, &args, &BTreeMap::new(), argv)
        })
    }

    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        Box::pin(async move {
            let argv = session_exec_argv(&self.podman_bin, spec)?;
            let args = argv[1..].to_vec();
            spawn_child_exec(&self.podman_bin, &args, &BTreeMap::new(), argv)
        })
    }

    fn stop<'a>(
        &'a self,
        container: &'a str,
        grace: Duration,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = stop_argv(&self.podman_bin, container, grace)?;
            self.spawn_ok(argv).await?;
            Ok(())
        })
    }

    fn kill<'a>(&'a self, container: &'a str) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = kill_argv(&self.podman_bin, container)?;
            self.spawn_ok(argv).await?;
            Ok(())
        })
    }

    fn state<'a>(
        &'a self,
        container: &'a str,
    ) -> BoxFuture<'a, Result<ContainerState, PodmanError>> {
        Box::pin(async move {
            let argv = state_argv(&self.podman_bin, container)?;
            let out = self.spawn_ok(argv).await?;
            Ok(parse_container_state(&out.stdout_lossy()))
        })
    }

    fn pull<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let exists = self
                .spawn_argv(image_exists_argv(&self.podman_bin, image)?, None)
                .await?;
            if exists.code == 0 {
                return Ok(()); // idempotent: already in the local store
            }
            let argv = pull_argv(&self.podman_bin, image)?;
            let mut attempt = 1;
            loop {
                match self.spawn_ok(argv.clone()).await {
                    Ok(_) => return Ok(()),
                    // Bounded retries with linear backoff — pkgcache-friendly:
                    // a flaky registry must not fail provisioning at once.
                    Err(e) if attempt >= self.pull_attempts => return Err(e),
                    Err(_) => {
                        tokio::time::sleep(self.pull_retry_delay * attempt).await;
                    }
                }
                attempt += 1;
            }
        })
    }

    fn remove<'a>(
        &'a self,
        container: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = remove_argv(&self.podman_bin, container, force)?;
            self.spawn_ok(argv).await?;
            Ok(())
        })
    }

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, PodmanError>> {
        Box::pin(async move {
            let argv = image_exists_argv(&self.podman_bin, image)?;
            let out = self.spawn_argv(argv.clone(), None).await?;
            match out.code {
                0 => Ok(true),
                1 => Ok(false),
                _ => Err(map_nonzero(out.code, &argv, out.stderr_lossy())),
            }
        })
    }

    fn build<'a>(
        &'a self,
        tag: &'a str,
        containerfile: &'a Path,
        context: &'a Path,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = build_argv(&self.podman_bin, tag, containerfile, context)?;
            self.spawn_ok(argv).await?;
            Ok(())
        })
    }

    fn unshare_chown<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = unshare_chown_argv(&self.podman_bin, uid, gid, paths)?;
            self.spawn_ok(argv).await?;
            Ok(())
        })
    }

    fn unshare_chown_recursive<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = unshare_chown_recursive_argv(&self.podman_bin, uid, gid, paths)?;
            self.spawn_ok(argv).await?;
            Ok(())
        })
    }

    fn unshare_exec<'a>(
        &'a self,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>> {
        Box::pin(async move {
            let argv = unshare_exec_argv(&self.podman_bin, cmd)?;
            let out = self.spawn_argv(argv.clone(), None).await?;
            if out.code == PODMAN_SELF_ERROR_CODE {
                return Err(map_nonzero(out.code, &argv, out.stderr_lossy()));
            }
            Ok(out)
        })
    }

    fn unshare_write_atomic<'a>(
        &'a self,
        dest: &'a Path,
        bytes: &'a [u8],
        owner: (u32, u32),
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let dir = dest.parent().ok_or_else(|| {
                PodmanError::invalid(format!("{} has no parent directory", dest.display()))
            })?;
            // 1. Stage the bytes in a daemon-private file the Mayor cannot
            //    reach: the daemon's own tmp dir, created O_EXCL, mode 0600.
            let staged = stage_bytes(bytes)?;
            let result = self.unshare_write_staged(dir, dest, &staged, owner).await;
            // The staging file is the daemon's alone; it never survives the
            // call, success or failure.
            let _ = tokio::fs::remove_file(&staged).await;
            result
        })
    }
}

impl PodmanRuntime {
    /// The four `podman unshare` steps of [`Runtime::unshare_write_atomic`]
    /// once `staged` holds the bytes: `mktemp` → `cp` → `chmod`/`chown` →
    /// `mv`.
    async fn unshare_write_staged(
        &self,
        dir: &Path,
        dest: &Path,
        staged: &Path,
        owner: (u32, u32),
    ) -> Result<(), PodmanError> {
        // (a) Exclusive, unpredictable staging name inside the target dir.
        let mktemp = mktemp_staged_cmd(dir, "hamlet-daemon-write")?;
        let out = self.unshare_exec(&mktemp).await?;
        if out.code != 0 {
            return Err(map_nonzero(out.code, &mktemp, out.stderr_lossy()));
        }
        let created = out.stdout_lossy();
        let created = created.trim_end_matches(['\n', '\r']).to_owned();
        // Trust nothing the tool printed: it must be an absolute path
        // *inside* the directory we asked about.
        let created_path = Path::new(&created);
        if !created_path.is_absolute()
            || created_path.parent() != Some(dir)
            || created.contains("..")
            || created.len() > 512
        {
            return Err(PodmanError::invalid(format!(
                "unshare mktemp returned an unusable path {created:?} (expected a fresh file in {})",
                dir.display()
            )));
        }

        // (b) .. (e) Any failure past this point must not leave the staging
        // file behind in a directory the Mayor owns: the daemon created it,
        // so the daemon removes it.
        let copy = cp_staged_cmd(staged, created_path)?;
        if let Err(e) = self.unshare_exec(&copy).await.and_then(check_exit(&copy)) {
            let _ = tokio::fs::remove_file(created_path).await;
            return Err(e);
        }
        let chmod = chmod_staged_cmd("0600", created_path)?;
        if let Err(e) = self.unshare_exec(&chmod).await.and_then(check_exit(&chmod)) {
            let _ = tokio::fs::remove_file(created_path).await;
            return Err(e);
        }
        let chown = chown_one_cmd(owner.0, owner.1, created_path)?;
        if let Err(e) = self.unshare_exec(&chown).await.and_then(check_exit(&chown)) {
            let _ = tokio::fs::remove_file(created_path).await;
            return Err(e);
        }
        let rename = mv_staged_cmd(created_path, dest)?;
        self.unshare_exec(&rename)
            .await
            .and_then(check_exit(&rename))?;
        Ok(())
    }
}

/// `unshare_exec` helper: a non-zero tool exit is an error carrying argv +
/// stderr (ADR-0010 cl. 5), never a silently ignored outcome.
fn check_exit(argv: &[String]) -> impl FnOnce(ExecOutput) -> Result<ExecOutput, PodmanError> + '_ {
    move |out: ExecOutput| {
        if out.code != 0 {
            return Err(map_nonzero(out.code, argv, out.stderr_lossy()));
        }
        Ok(out)
    }
}

/// Stage `bytes` in a daemon-private temp file: `O_CREAT|O_EXCL`, mode 0600,
/// fsynced before the read that the `cp` step will perform. Never in the
/// village tree — the destination is in a directory the Mayor controls, and
/// this file has no business being visible there.
fn stage_bytes(bytes: &[u8]) -> Result<PathBuf, PodmanError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = std::env::temp_dir();
    for attempt in 0..256u32 {
        let candidate = dir.join(format!(
            "hamlet-daemon-write-{}-{attempt}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(PodmanError::spawn(
                    e.to_string(),
                    vec![candidate.display().to_string()],
                ))
            }
        };
        if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = std::fs::remove_file(&candidate);
            return Err(PodmanError::spawn(e.to_string(), vec![]));
        }
        return Ok(candidate);
    }
    Err(PodmanError::invalid(
        "no free staging file name after 256 attempts",
    ))
}

/// One recorded [`Runtime::unshare_write_atomic`] call: where, what, and who
/// the file is handed to (QA05-02).
pub type UnshareWrite = (PathBuf, Vec<u8>, (u32, u32));

/// Real interactive exec session wrapping the tokio child.
struct ChildExecHandle {
    child: Child,
    pipes: Option<ExecPipes>,
    argv: Vec<String>,
}

impl ExecHandle for ChildExecHandle {
    fn take_pipes(&mut self) -> Option<ExecPipes> {
        self.pipes.take()
    }

    fn kill(&mut self) -> Result<(), PodmanError> {
        self.child
            .start_kill()
            .map_err(|e| PodmanError::spawn(e.to_string(), self.argv.clone()))
    }

    fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<i32, PodmanError>> {
        Box::pin(async move {
            let status = self
                .child
                .wait()
                .await
                .map_err(|e| PodmanError::spawn(e.to_string(), self.argv.clone()))?;
            Ok(status.code().unwrap_or(SIGNAL_EXIT_CODE))
        })
    }
}

// ---------------------------------------------------------------------------
// FakeRuntime (test seam — records argv, spawns nothing)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeScript {
    failures: VecDeque<PodmanErrorKind>,
    verb_failures: BTreeMap<String, VecDeque<PodmanErrorKind>>,
    state: ContainerState,
    exec_output: ExecOutput,
    exec_scripts: Vec<(String, ExecOutput)>,
    image_exists: bool,
    run_id: String,
    exec_handle_stdout: Vec<u8>,
    exec_handle_code: i32,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            failures: VecDeque::new(),
            verb_failures: BTreeMap::new(),
            state: ContainerState::Running,
            exec_output: ExecOutput::default(),
            exec_scripts: Vec::new(),
            image_exists: true,
            run_id: "fake0container0id".to_owned(),
            exec_handle_stdout: Vec::new(),
            exec_handle_code: 0,
        }
    }
}

/// The podman test seam (CONVENTIONS §9): builds argv through **the same
/// pure builders** as [`PodmanRuntime`] (so argv goldens and the
/// forbidden-flag audit cover every spawn path), records every argv, and
/// returns scripted results without touching podman.
///
/// Failure injection: [`FakeRuntime::fail_next`] queues a
/// [`PodmanErrorKind`] consumed by the next runtime call — used by Sprint 02
/// step 03 rollback tests.
#[derive(Debug)]
pub struct FakeRuntime {
    podman_bin: PathBuf,
    recorded: Mutex<Vec<Vec<String>>>,
    recorded_stdin: Mutex<Vec<(Vec<String>, Vec<u8>)>>,
    /// Every [`Runtime::unshare_write_atomic`] call: destination, bytes and
    /// the owner the file is handed to (QA05-02).
    unshare_writes: Mutex<Vec<UnshareWrite>>,
    script: Mutex<FakeScript>,
}

impl Default for FakeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRuntime {
    /// A fresh fake with success-by-default scripting.
    #[must_use]
    pub fn new() -> Self {
        Self {
            podman_bin: PathBuf::from("podman"),
            recorded: Mutex::new(Vec::new()),
            recorded_stdin: Mutex::new(Vec::new()),
            unshare_writes: Mutex::new(Vec::new()),
            script: Mutex::new(FakeScript::default()),
        }
    }

    /// Every argv recorded so far, in call order (already redacted).
    #[must_use]
    pub fn argv_log(&self) -> Vec<Vec<String>> {
        self.lock(&self.recorded).clone()
    }

    /// Every [`Runtime::unshare_write_atomic`] call recorded so far:
    /// `(destination, bytes, owner)`, in call order.
    #[must_use]
    pub fn unshare_writes(&self) -> Vec<UnshareWrite> {
        self.lock(&self.unshare_writes).clone()
    }

    /// Every `exec` stdin payload recorded so far, paired with its (redacted)
    /// argv, in call order. Sprint 02 step 04 needs this to prove that the
    /// bytes bootstrap writes into a home are exactly the plan's — and that
    /// no secret ever rides along (CONVENTIONS §6).
    #[must_use]
    pub fn stdin_log(&self) -> Vec<(Vec<String>, Vec<u8>)> {
        self.lock(&self.recorded_stdin).clone()
    }

    /// Queue one injected failure for the next runtime call.
    pub fn fail_next(&self, kind: PodmanErrorKind) {
        self.lock(&self.script).failures.push_back(kind);
    }

    /// Queue one injected failure for the next runtime call whose podman
    /// subcommand (argv[1]) equals `verb` — e.g. `"run"`, `"pull"`,
    /// `"unshare"`, `"rm"`. Verb queues are checked before the generic
    /// [`FakeRuntime::fail_next`] queue, so provisioning rollback tests can
    /// fail exactly one step of a multi-call flow.
    pub fn fail_next_for(&self, verb: &str, kind: PodmanErrorKind) {
        self.lock(&self.script)
            .verb_failures
            .entry(verb.to_owned())
            .or_default()
            .push_back(kind);
    }

    /// Script the [`ContainerState`] returned by `state`.
    pub fn set_state(&self, state: ContainerState) {
        self.lock(&self.script).state = state;
    }

    /// Script the [`ExecOutput`] returned by `exec`.
    pub fn set_exec_output(&self, output: ExecOutput) {
        self.lock(&self.script).exec_output = output;
    }

    /// Script the [`ExecOutput`] for `exec` calls whose in-container command
    /// (joined with single spaces) CONTAINS `matcher`. Scripts registered
    /// earlier win; anything unmatched falls back to
    /// [`FakeRuntime::set_exec_output`].
    ///
    /// Bootstrap needs this because one flow issues many distinct execs
    /// (`cat` a manifest, `sha256sum` an artifact, `tar -xJf …`, probe a
    /// `--version`) that must each answer differently — the seam stays
    /// generic (no bootstrap-specific knowledge in the fake).
    pub fn script_exec_for(&self, matcher: impl Into<String>, output: ExecOutput) {
        self.lock(&self.script)
            .exec_scripts
            .push((matcher.into(), output));
    }

    /// Script whether `podman image exists` succeeds (default: true →
    /// `pull` records only the exists probe).
    pub fn set_image_exists(&self, exists: bool) {
        self.lock(&self.script).image_exists = exists;
    }

    /// Script the container ID returned by `run_detached`.
    pub fn set_run_id(&self, id: impl Into<String>) {
        self.lock(&self.script).run_id = id.into();
    }

    /// Script the stdout bytes / exit code of `spawn_exec` handles.
    pub fn set_exec_handle(&self, stdout: Vec<u8>, code: i32) {
        let mut script = self.lock(&self.script);
        script.exec_handle_stdout = stdout;
        script.exec_handle_code = code;
    }

    fn lock<'m, T>(&self, mutex: &'m Mutex<T>) -> MutexGuard<'m, T> {
        mutex.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, argv: Vec<String>) -> Vec<String> {
        let redacted = redacted_argv(&argv);
        self.lock(&self.recorded).push(redacted.clone());
        redacted
    }

    fn fake_handle(&self, argv: Vec<String>) -> Result<Box<dyn ExecHandle>, PodmanError> {
        let recorded = self.record(argv);
        if let Some(err) = self.take_failure(&recorded) {
            return Err(err);
        }
        let script = self.lock(&self.script);
        Ok(Box::new(FakeExecHandle::new(
            script.exec_handle_stdout.clone(),
            script.exec_handle_code,
        )) as Box<dyn ExecHandle>)
    }

    fn take_failure(&self, argv: &[String]) -> Option<PodmanError> {
        let mut script = self.lock(&self.script);
        let verb = argv.get(1).map(String::as_str).unwrap_or("");
        let kind = script
            .verb_failures
            .get_mut(verb)
            .and_then(VecDeque::pop_front)
            .or_else(|| script.failures.pop_front())?;
        Some(PodmanError {
            kind,
            argv: argv.to_vec(),
            stderr: "fake: injected failure".to_owned(),
        })
    }
}

impl Runtime for FakeRuntime {
    fn run_detached<'a>(
        &'a self,
        spec: &'a RunSpec,
        tree: &'a HamletTree,
    ) -> BoxFuture<'a, Result<String, PodmanError>> {
        Box::pin(async move {
            let argv = spec.to_run_argv(tree, &self.podman_bin)?;
            let recorded = self.record(argv.clone());
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            Ok(self.lock(&self.script).run_id.clone())
        })
    }

    fn exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>> {
        Box::pin(async move {
            let argv = exec_argv(&self.podman_bin, container, cmd, stdin.is_some())?;
            let recorded = self.record(argv.clone());
            if let Some(payload) = stdin {
                self.lock(&self.recorded_stdin)
                    .push((recorded.clone(), payload.to_vec()));
            }
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            let joined = cmd.join(" ");
            let script = self.lock(&self.script);
            if let Some((_, output)) = script
                .exec_scripts
                .iter()
                .find(|(matcher, _)| joined.contains(matcher.as_str()))
            {
                return Ok(output.clone());
            }
            Ok(script.exec_output.clone())
        })
    }

    fn spawn_exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        Box::pin(async move {
            let argv = exec_argv(&self.podman_bin, container, cmd, true)?;
            self.fake_handle(argv)
        })
    }

    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        Box::pin(async move {
            let argv = session_exec_argv(&self.podman_bin, spec)?;
            self.fake_handle(argv)
        })
    }

    fn stop<'a>(
        &'a self,
        container: &'a str,
        grace: Duration,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = stop_argv(&self.podman_bin, container, grace)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            self.lock(&self.script).state = ContainerState::Exited;
            Ok(())
        })
    }

    fn kill<'a>(&'a self, container: &'a str) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = kill_argv(&self.podman_bin, container)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            self.lock(&self.script).state = ContainerState::Exited;
            Ok(())
        })
    }

    fn state<'a>(
        &'a self,
        container: &'a str,
    ) -> BoxFuture<'a, Result<ContainerState, PodmanError>> {
        Box::pin(async move {
            let argv = state_argv(&self.podman_bin, container)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            Ok(self.lock(&self.script).state)
        })
    }

    fn pull<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let exists_argv = image_exists_argv(&self.podman_bin, image)?;
            let recorded_exists = self.record(exists_argv);
            let exists = self.lock(&self.script).image_exists;
            if exists {
                if let Some(err) = self.take_failure(&recorded_exists) {
                    return Err(err);
                }
                return Ok(());
            }
            let pull_argv = pull_argv(&self.podman_bin, image)?;
            let recorded_pull = self.record(pull_argv);
            if let Some(err) = self.take_failure(&recorded_pull) {
                return Err(err);
            }
            Ok(())
        })
    }

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, PodmanError>> {
        Box::pin(async move {
            let argv = image_exists_argv(&self.podman_bin, image)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            Ok(self.lock(&self.script).image_exists)
        })
    }

    fn build<'a>(
        &'a self,
        tag: &'a str,
        containerfile: &'a Path,
        context: &'a Path,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = build_argv(&self.podman_bin, tag, containerfile, context)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            Ok(())
        })
    }

    fn remove<'a>(
        &'a self,
        container: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = remove_argv(&self.podman_bin, container, force)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            self.lock(&self.script).state = ContainerState::Exited;
            Ok(())
        })
    }

    fn unshare_chown<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = unshare_chown_argv(&self.podman_bin, uid, gid, paths)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            Ok(())
        })
    }

    fn unshare_chown_recursive<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            let argv = unshare_chown_recursive_argv(&self.podman_bin, uid, gid, paths)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            Ok(())
        })
    }

    fn unshare_exec<'a>(
        &'a self,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>> {
        Box::pin(async move {
            let argv = unshare_exec_argv(&self.podman_bin, cmd)?;
            let recorded = self.record(argv);
            if let Some(err) = self.take_failure(&recorded) {
                return Err(err);
            }
            let joined = cmd.join(" ");
            let script = self.lock(&self.script);
            if let Some((_, output)) = script
                .exec_scripts
                .iter()
                .find(|(matcher, _)| joined.contains(matcher.as_str()))
            {
                return Ok(output.clone());
            }
            Ok(script.exec_output.clone())
        })
    }

    /// The fake write seam: record `(dest, bytes, owner)` and then perform
    /// the write the real runtime's five steps would have performed, so a
    /// test can assert both *which* payload reached the seam and the
    /// resulting tree. No podman, no userns.
    fn unshare_write_atomic<'a>(
        &'a self,
        dest: &'a Path,
        bytes: &'a [u8],
        owner: (u32, u32),
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        Box::pin(async move {
            self.lock(&self.unshare_writes)
                .push((dest.to_path_buf(), bytes.to_vec(), owner));
            // The real seam writes as userns **root**, which is not subject
            // to the DAC bits of a subuid-owned 0700 home (ADR-0011 cl. 4).
            // The fake has no userns, so it emulates exactly that one
            // property: if the ordinary write is refused, the parent
            // directory is opened up for the duration and restored after.
            match std::fs::write(dest, bytes) {
                Ok(()) => Ok(()),
                Err(refused) if refused.kind() == std::io::ErrorKind::PermissionDenied => {
                    use std::os::unix::fs::PermissionsExt;
                    let parent = dest.parent().ok_or_else(|| {
                        PodmanError::invalid("fake unshare write: no parent directory")
                    })?;
                    let original = std::fs::metadata(parent)
                        .map_err(|e| PodmanError::invalid(format!("fake unshare stat: {e}")))?
                        .permissions()
                        .mode()
                        & 0o7777;
                    std::fs::set_permissions(
                        parent,
                        std::fs::Permissions::from_mode(original | 0o700),
                    )
                    .map_err(|e| PodmanError::invalid(format!("fake unshare chmod: {e}")))?;
                    let written = std::fs::write(dest, bytes);
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(original))
                        .map_err(|e| {
                            PodmanError::invalid(format!("fake unshare chmod back: {e}"))
                        })?;
                    written.map_err(|e| PodmanError::invalid(format!("fake unshare write: {e}")))
                }
                Err(e) => Err(PodmanError::invalid(format!("fake unshare write: {e}"))),
            }
        })
    }
}

/// In-memory [`ExecHandle`] handed out by [`FakeRuntime`]: stdin writes
/// accumulate in a shared `Vec<u8>`; stdout replays the scripted bytes;
/// `wait` resolves to the scripted exit code ([`SIGNAL_EXIT_CODE`] after
/// [`ExecHandle::kill`]).
struct FakeExecHandle {
    stdin: Arc<Mutex<Vec<u8>>>,
    stdout: Option<Cursor<Vec<u8>>>,
    stderr: Option<Cursor<Vec<u8>>>,
    code: i32,
    killed: bool,
}

impl FakeExecHandle {
    fn new(stdout: Vec<u8>, code: i32) -> Self {
        Self {
            stdin: Arc::new(Mutex::new(Vec::new())),
            stdout: Some(Cursor::new(stdout)),
            stderr: Some(Cursor::new(Vec::new())),
            code,
            killed: false,
        }
    }
}

/// [`AsyncWrite`] adapter appending into the fake's shared stdin buffer.
#[derive(Debug)]
struct FakeStdin(Arc<Mutex<Vec<u8>>>);

impl AsyncWrite for FakeStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl ExecHandle for FakeExecHandle {
    fn take_pipes(&mut self) -> Option<ExecPipes> {
        let stdout = self.stdout.take()?;
        Some(ExecPipes {
            stdin: Box::new(FakeStdin(Arc::clone(&self.stdin))),
            stdout: Box::new(stdout),
            stderr: Box::new(
                self.stderr
                    .take()
                    .unwrap_or_else(|| Cursor::new(Vec::new())),
            ),
        })
    }

    fn kill(&mut self) -> Result<(), PodmanError> {
        self.killed = true;
        Ok(())
    }

    fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<i32, PodmanError>> {
        let code = if self.killed {
            SIGNAL_EXIT_CODE
        } else {
            self.code
        };
        Box::pin(async move { Ok(code) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamlet_core::blueprint::Blueprint;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn tree() -> HamletTree {
        HamletTree::new("/village/.hamlet")
    }

    fn bin() -> PathBuf {
        PathBuf::from("podman")
    }

    fn blueprint() -> Blueprint {
        Blueprint::default_village_alpine().expect("committed default blueprint parses")
    }

    fn config() -> Config {
        Config::default()
    }

    fn agent_spec() -> RunSpec {
        RunSpec::for_agent(
            &sanitize_name("scribe").unwrap(),
            1001,
            3,
            &blueprint(),
            &config(),
        )
        .expect("1001 is a valid slot")
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Golden vector: the complete `podman run` argv for an agent container
    /// (default blueprint + default config) — hardening flags, userns
    /// mapping, resource limits, SPEC §4 mounts in table order, sorted env.
    #[test]
    fn run_argv_golden_agent() {
        let spec = agent_spec();
        let argv = spec.to_run_argv(&tree(), &bin()).unwrap();
        let uid = 1001u32;
        let mut expected = vec![
            "podman".to_owned(),
            "run".to_owned(),
            "-d".to_owned(),
            "--name".to_owned(),
            "scribe".to_owned(),
            "--hostname".to_owned(),
            format!("parcel{uid}"),
            // ADR-0011: default rootless subuid userns — NO --userns flag.
            "--user".to_owned(),
            format!("{uid}:{uid}"),
            // The SPEC §4 home mount: sessions and bootstrap start in the
            // home, and HOME is set because a numeric --user has no passwd
            // entry (ADR-0012 cl. 3).
            "--workdir".to_owned(),
            format!("/home/{uid}"),
            "--security-opt=no-new-privileges".to_owned(),
            "--cap-drop=ALL".to_owned(),
            "--memory".to_owned(),
            "512m".to_owned(),
            "--cpus".to_owned(),
            "1.0".to_owned(),
            "--pids-limit".to_owned(),
            "256".to_owned(),
        ];
        expected.extend(spec.mounts.to_podman_args(&tree()).unwrap());
        for (k, v) in &spec.env {
            expected.push("-e".to_owned());
            expected.push(format!("{k}={v}"));
        }
        assert!(
            spec.env.contains_key("HOME"),
            "HOME is daemon-set for every container"
        );
        expected.push("docker.io/library/alpine:3.24".to_owned());
        expected.push("sleep".to_owned());
        expected.push("infinity".to_owned());
        assert_eq!(argv, expected);
    }

    /// Golden vector: Mayor run argv — UID 1000, manor hostname, no git.
    #[test]
    fn run_argv_golden_mayor() {
        let spec =
            RunSpec::for_mayor(&sanitize_name("mayor").unwrap(), &blueprint(), &config()).unwrap();
        let argv = spec.to_run_argv(&tree(), &bin()).unwrap();
        assert!(
            !argv.iter().any(|a| a.starts_with("--userns")),
            "ADR-0011: default rootless userns, never an explicit --userns flag"
        );
        assert!(argv.contains(&"1000:1000".to_owned()));
        assert!(argv.contains(&"manor".to_owned()));
        assert!(
            !argv.iter().any(|a| a.contains("/var/git")),
            "Mayor must not bind commission git"
        );
        assert!(argv
            .iter()
            .any(|a| a == "/village/.hamlet/keep/census/manor/home:/home/1000/"));
        assert_eq!(spec.workdir, "/home/1000", "the ManorHome mount row");
        assert_eq!(spec.env.get("HOME").map(String::as_str), Some("/home/1000"));
    }

    /// `HOME` and `--workdir` are pinned to the SPEC §4 home mount: a
    /// hand-assembled spec that moves either is rejected at argv build, so
    /// no caller can park a session outside the parcel home.
    #[test]
    fn home_and_workdir_are_pinned_to_the_spec4_home_mount() {
        assert_eq!(home_container_dir(1001), "/home/1001");
        assert_eq!(home_container_dir(MAYOR_UID), "/home/1000");

        let mut spec = agent_spec();
        spec.workdir = "/tmp".to_owned();
        let err = spec.to_run_argv(&tree(), &bin()).unwrap_err();
        assert!(err.to_string().contains("workdir"), "{err}");

        let mut spec = agent_spec();
        spec.env
            .insert("HOME".to_owned(), "/var/pkgcache".to_owned());
        let err = spec.to_run_argv(&tree(), &bin()).unwrap_err();
        assert!(err.to_string().contains("HOME"), "{err}");

        let mut spec = agent_spec();
        spec.env.remove("HOME");
        assert!(spec.to_run_argv(&tree(), &bin()).is_err());

        // The derived path is the only accepted value.
        assert!(agent_spec().to_run_argv(&tree(), &bin()).is_ok());
    }

    /// `with_image` swaps in the derived parcel image (ADR-0012) and the
    /// value is re-validated — a leading-dash or whitespace tag dies here.
    #[test]
    fn with_image_overrides_and_revalidates() {
        let spec = agent_spec().with_image("localhost/hamlet/village-alpine:v1-f1-c0");
        let argv = spec.to_run_argv(&tree(), &bin()).unwrap();
        assert!(argv.contains(&"localhost/hamlet/village-alpine:v1-f1-c0".to_owned()));
        assert!(!argv.contains(&"docker.io/library/alpine:3.24".to_owned()));
        for bad in ["-x", "", "a b", "--privileged"] {
            assert!(
                agent_spec()
                    .with_image(bad)
                    .to_run_argv(&tree(), &bin())
                    .is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    /// Adversarial audit over every argv this module can build: no
    /// forbidden flag or socket mount ever appears, and the required
    /// hardening flags always do (ADR-0010 cl. 4; CONVENTIONS §5).
    #[tokio::test]
    async fn no_forbidden_flags_in_any_recorded_argv() {
        let fake = FakeRuntime::new();
        let spec = agent_spec();
        let t = tree();
        fake.run_detached(&spec, &t).await.unwrap();
        fake.exec("scribe", &cmd(&["id", "-u"]), None)
            .await
            .unwrap();
        fake.spawn_exec("scribe", &cmd(&["pi", "--stdio"]))
            .await
            .unwrap();
        fake.stop("scribe", Duration::from_secs(5)).await.unwrap();
        fake.kill("scribe").await.unwrap();
        fake.state("scribe").await.unwrap();
        fake.set_image_exists(false);
        fake.pull("docker.io/library/alpine:3.20").await.unwrap();
        fake.image_exists("localhost/hamlet/x:v1").await.unwrap();
        fake.build(
            "localhost/hamlet/x:v1",
            &PathBuf::from("/tmp/hamlet-build-x/Containerfile"),
            &PathBuf::from("/tmp/hamlet-build-x"),
        )
        .await
        .unwrap();
        fake.remove("scribe", true).await.unwrap();
        fake.unshare_chown(1001, 1001, &[PathBuf::from("/village/.hamlet/x")])
            .await
            .unwrap();

        let log = fake.argv_log();
        assert_eq!(log.len(), 12, "every spawn path recorded");
        for argv in &log {
            let joined = argv.join(" ");
            for forbidden in [
                "--privileged",
                "--userns=host",
                "--pid=host",
                "--network=host",
                "podman.sock",
                "docker.sock",
                ":/run",
                ":/var/run",
                "--authfile",
                "sh -c",
            ] {
                assert!(
                    !joined.contains(forbidden),
                    "forbidden {forbidden:?} in {joined}"
                );
            }
            assert!(argv[0] == "podman", "argv[0] is always the binary");
        }
        // Required hardening on the run path.
        let run = &log[0];
        assert!(run.contains(&"--security-opt=no-new-privileges".to_owned()));
        assert!(run.contains(&"--cap-drop=ALL".to_owned()));
        assert!(run.contains(&"--user".to_owned()));
        assert!(run.contains(&"1001:1001".to_owned()));
        // ADR-0011 (amending ADR-0010 cl. 3): the default rootless subuid
        // userns is used — an explicit --userns flag must never appear.
        assert!(
            !run.iter().any(|a| a.starts_with("--userns")),
            "no --userns flag on any run argv"
        );
    }

    /// The ro invariants are visible in argv: `/var/git/` and
    /// `/var/context/` render with `:ro`; homes/mail/freight/pkgcache rw.
    #[test]
    fn ro_invariants_in_run_argv() {
        let argv = agent_spec().to_run_argv(&tree(), &bin()).unwrap();
        let volumes: Vec<&str> = argv
            .iter()
            .zip(argv.iter().skip(1))
            .filter(|(a, _)| a.as_str() == "-v")
            .map(|(_, v)| v.as_str())
            .collect();
        assert!(volumes.iter().any(|v| v.ends_with("/var/git/:ro")));
        assert!(volumes.iter().any(|v| v.ends_with("/var/context/:ro")));
        assert!(volumes
            .iter()
            .any(|v| v.ends_with("/home/1001/") && !v.ends_with(":ro")));
        assert!(volumes
            .iter()
            .any(|v| v.ends_with("/var/mail/") && !v.ends_with(":ro")));
        assert!(volumes
            .iter()
            .any(|v| v.ends_with("/var/freight/") && !v.ends_with(":ro")));
        assert!(volumes
            .iter()
            .any(|v| v.ends_with("/var/pkgcache/") && !v.ends_with(":ro")));
        assert_eq!(volumes.len(), 6, "exactly the SPEC §4 agent rows");
    }

    #[test]
    fn lifecycle_argv_goldens() {
        assert_eq!(
            exec_argv(&bin(), "scribe", &cmd(&["id", "-u"]), false).unwrap(),
            vec!["podman", "exec", "scribe", "id", "-u"]
        );
        assert_eq!(
            exec_argv(&bin(), "scribe", &cmd(&["cat"]), true).unwrap(),
            vec!["podman", "exec", "-i", "scribe", "cat"]
        );
        assert_eq!(
            stop_argv(&bin(), "scribe", Duration::from_secs(10)).unwrap(),
            vec!["podman", "stop", "--time", "10", "scribe"]
        );
        assert_eq!(
            kill_argv(&bin(), "scribe").unwrap(),
            vec!["podman", "kill", "scribe"]
        );
        assert_eq!(
            state_argv(&bin(), "scribe").unwrap(),
            vec![
                "podman",
                "inspect",
                "--format",
                "{{.State.Status}}",
                "scribe"
            ]
        );
        assert_eq!(
            image_exists_argv(&bin(), "alpine:3.20").unwrap(),
            vec!["podman", "image", "exists", "alpine:3.20"]
        );
        assert_eq!(
            pull_argv(&bin(), "alpine:3.20").unwrap(),
            vec!["podman", "pull", "alpine:3.20"]
        );
        // ADR-0012 cl. 2 golden: derived-image build argv.
        assert_eq!(
            build_argv(
                &bin(),
                "localhost/hamlet/village-alpine:v1-f1-c0",
                &PathBuf::from("/tmp/hamlet-build-village-alpine/Containerfile"),
                &PathBuf::from("/tmp/hamlet-build-village-alpine"),
            )
            .unwrap(),
            vec![
                "podman",
                "build",
                "--tag",
                "localhost/hamlet/village-alpine:v1-f1-c0",
                "--file",
                "/tmp/hamlet-build-village-alpine/Containerfile",
                "/tmp/hamlet-build-village-alpine",
            ]
        );
        for bad in [
            PathBuf::from("relative/Containerfile"),
            PathBuf::from("-f"),
            PathBuf::from(""),
            PathBuf::from("/tmp/has\nnewline"),
        ] {
            assert!(
                build_argv(&bin(), "localhost/x:v1", &bad, &PathBuf::from("/tmp")).is_err(),
                "{} must be refused",
                bad.display()
            );
            assert!(
                build_argv(&bin(), "localhost/x:v1", &PathBuf::from("/tmp/c"), &bad).is_err(),
                "context {} must be refused",
                bad.display()
            );
        }
        assert_eq!(
            remove_argv(&bin(), "scribe", true).unwrap(),
            vec!["podman", "rm", "--force", "scribe"]
        );
        assert_eq!(
            remove_argv(&bin(), "scribe", false).unwrap(),
            vec!["podman", "rm", "scribe"]
        );
        // ADR-0011 cl. 2 golden: unshare chown with the `--` guard.
        assert_eq!(
            unshare_chown_argv(
                &bin(),
                1001,
                1001,
                &[
                    PathBuf::from("/v/.hamlet/keep/census/parcel1001/home"),
                    PathBuf::from("/v/.hamlet/courier/mail/1001"),
                ]
            )
            .unwrap(),
            vec![
                "podman",
                "unshare",
                "chown",
                "1001:1001",
                "--",
                "/v/.hamlet/keep/census/parcel1001/home",
                "/v/.hamlet/courier/mail/1001",
            ]
        );
    }

    /// Host paths in `unshare chown` argv are positional slots: relative,
    /// empty, control-char, and leading-dash paths must die pre-flight.
    #[test]
    fn unshare_chown_path_injection_rejected() {
        assert!(unshare_chown_argv(&bin(), 1001, 1001, &[]).is_err());
        for evil in [
            "--reference=/etc/passwd",
            "-R",
            "relative/path",
            "",
            "/v/has\nnewline",
        ] {
            assert!(
                unshare_chown_argv(&bin(), 1001, 1001, &[PathBuf::from(evil)]).is_err(),
                "{evil:?} must be rejected"
            );
            assert!(
                unshare_chown_recursive_argv(&bin(), 1001, 1001, &[PathBuf::from(evil)]).is_err(),
                "the recursive brace validates identically: {evil:?} must be rejected"
            );
        }
        // Valid absolute path passes.
        assert!(unshare_chown_argv(&bin(), 1001, 1001, &[PathBuf::from("/v/.hamlet/x")]).is_ok());
        assert!(
            unshare_chown_recursive_argv(&bin(), 1001, 1001, &[PathBuf::from("/v/.hamlet/x")])
                .is_ok()
        );
    }

    /// ADR-0034 golden: the recursive brace is the same argv with `-RP`
    /// between `chown` and the ownership spec, and the `--` guard stays.
    #[test]
    fn unshare_chown_recursive_argv_golden() {
        assert!(unshare_chown_recursive_argv(&bin(), 1000, 1000, &[]).is_err());
        assert_eq!(
            unshare_chown_recursive_argv(
                &bin(),
                1000,
                1000,
                &[PathBuf::from("/v/.hamlet/keep/census/manor/home")],
            )
            .unwrap(),
            vec![
                "podman",
                "unshare",
                "chown",
                "-RP",
                "1000:1000",
                "--",
                "/v/.hamlet/keep/census/manor/home",
            ]
        );
    }

    /// **QA04-29 / ADR-0034 cl. 5 — the recursive brace must not
    /// dereference.** The manor home is agent-writable, so the tree the
    /// `-R` walk descends contains symlinks the adversary chose. Whether
    /// `-R` follows them is the difference between chowning the Mayor's own
    /// subtree and chowning an arbitrary host path as the Mayor's subuid.
    ///
    /// The safety used to be an unpinned GNU default (`-P`), so this pins
    /// **both directions**: the flag must be present, and no dereferencing
    /// flag (`-L`, `-H`, or a `--dereference`-style long option) may appear
    /// anywhere in the argv. A future "-L is more correct" edit fails here.
    #[test]
    fn chown_recursive_argv_pins_no_dereference() {
        let argv = unshare_chown_recursive_argv(
            &bin(),
            1000,
            1000,
            &[PathBuf::from("/v/.hamlet/keep/census/manor/home")],
        )
        .unwrap();
        assert_eq!(
            argv.iter()
                .filter(|a| a.starts_with('-') && a.as_str() != "--")
                .cloned()
                .collect::<Vec<_>>(),
            vec!["-RP".to_owned()],
            "the recursive chown's whole option set is `-RP` (ADR-0034 cl. 5)"
        );
        for forbidden in [
            "-L",
            "-H",
            "--dereference",
            "--no-dereference",
            "-RL",
            "-RH",
        ] {
            assert!(
                !argv.iter().any(|a| a == forbidden),
                "`chown` must never be asked to dereference ({forbidden}): {argv:?}"
            );
        }
        // The non-recursive brace touches the path itself only; it carries
        // no traversal flag at all, so the two forms cannot be confused.
        let flat = unshare_chown_argv(
            &bin(),
            1000,
            1000,
            &[PathBuf::from("/v/.hamlet/keep/census/manor/home")],
        )
        .unwrap();
        assert_eq!(
            flat.iter()
                .filter(|a| a.starts_with('-') && a.as_str() != "--")
                .count(),
            0,
            "the non-recursive brace has no traversal options: {flat:?}"
        );
    }

    /// QA04-12: the audit cannot do its job without the `# owner:` line, and
    /// `-c`/`--omit-header` is exactly what removes it. Pinned as an
    /// invariant so a tidy-up never re-adds it.
    #[test]
    fn getfacl_payload_keeps_the_owner_header() {
        let argv = getfacl_cmd(Path::new("/v/.hamlet/courier/freight/exchange")).unwrap();
        assert!(!argv.iter().any(|a| a == "-c" || a == "--omit-header"));
        assert!(argv.contains(&"-p".to_owned()) && argv.contains(&"-n".to_owned()));
    }

    /// ADR-0021 cl. 2 goldens: the typed `podman unshare` payload builders
    /// (freight ACL audit / root grants / promotion-read).
    #[test]
    fn unshare_payload_argv_goldens() {
        assert_eq!(
            unshare_exec_argv(&bin(), &["getfacl".to_owned(), "--".to_owned()]).unwrap(),
            vec!["podman", "unshare", "getfacl", "--"]
        );
        assert_eq!(
            stat_cmd(Path::new("/v/.hamlet/courier/freight/exchange/leak.md")).unwrap(),
            vec![
                "stat",
                "-c",
                "%F",
                "--",
                "/v/.hamlet/courier/freight/exchange/leak.md"
            ]
        );
        assert_eq!(
            getfacl_cmd(Path::new("/v/.hamlet/courier/freight/exchange")).unwrap(),
            vec![
                "getfacl",
                "-p",
                "-n",
                "--",
                "/v/.hamlet/courier/freight/exchange"
            ]
        );
        assert_eq!(
            setfacl_cmd(
                &[
                    hamlet_core::freight::AclGrant::root_access(1001).token(),
                    hamlet_core::freight::AclGrant::root_access(1002).token(),
                ],
                Path::new("/v/.hamlet/courier/freight")
            )
            .unwrap(),
            vec![
                "setfacl",
                "-m",
                "u:1001:rwx",
                "-m",
                "u:1002:rwx",
                "--",
                "/v/.hamlet/courier/freight"
            ]
        );
        assert_eq!(
            cat_cmd(Path::new("/v/.hamlet/courier/freight/exchange/payload.bin")).unwrap(),
            vec![
                "cat",
                "--",
                "/v/.hamlet/courier/freight/exchange/payload.bin"
            ]
        );
    }

    /// ADR-0011 cl. 4 / QA05-02 goldens: the five typed payload builders the
    /// unshare **write** seam composes. Pinned because the whole point of
    /// the seam is that the Mayor owns the destination directory — every
    /// name it derives must be `mktemp`'s exclusive, unpredictable one, and
    /// every path slot must be behind `--`.
    #[test]
    fn unshare_write_payload_argv_goldens() {
        let dir = Path::new("/v/.hamlet/keep/census/manor/home/decrees");
        let staged = Path::new("/tmp/hamlet-daemon-write-1234-0-99");
        let created =
            Path::new("/v/.hamlet/keep/census/manor/home/decrees/.hamlet-daemon-writeAbC123");
        assert_eq!(
            mktemp_staged_cmd(dir, "hamlet-daemon-write").unwrap(),
            vec![
                "mktemp",
                "--",
                "/v/.hamlet/keep/census/manor/home/decrees/.hamlet-daemon-writeXXXXXX",
            ]
        );
        assert_eq!(
            cp_staged_cmd(staged, created).unwrap(),
            vec![
                "cp",
                "--",
                "/tmp/hamlet-daemon-write-1234-0-99",
                "/v/.hamlet/keep/census/manor/home/decrees/.hamlet-daemon-writeAbC123",
            ]
        );
        assert_eq!(
            chmod_staged_cmd("0600", created).unwrap(),
            vec![
                "chmod",
                "0600",
                "--",
                "/v/.hamlet/keep/census/manor/home/decrees/.hamlet-daemon-writeAbC123",
            ]
        );
        assert_eq!(
            chown_one_cmd(1000, 1000, created).unwrap(),
            vec![
                "chown",
                "1000:1000",
                "--",
                "/v/.hamlet/keep/census/manor/home/decrees/.hamlet-daemon-writeAbC123",
            ]
        );
        assert_eq!(
            mv_staged_cmd(
                created,
                Path::new("/v/.hamlet/keep/census/manor/home/decrees/d-000000000007-a1b2.ack")
            )
            .unwrap(),
            vec![
                "mv",
                "--",
                "/v/.hamlet/keep/census/manor/home/decrees/.hamlet-daemon-writeAbC123",
                "/v/.hamlet/keep/census/manor/home/decrees/d-000000000007-a1b2.ack",
            ]
        );
    }

    /// The write-seam builders are typed sources and hold the same
    /// injection guarantees as every other argv builder (ADR-0010 cl. 1):
    /// a hostile prefix cannot become a template, a relative path cannot
    /// become an operand, and a sloppy mode cannot become a chmod.
    #[test]
    fn unshare_write_payload_injection_rejected() {
        let dir = Path::new("/v/.hamlet/keep/census/manor/home/decrees");
        for evil in ["../../etc", "", "-t", "/v/has\nnewline"] {
            assert!(
                mktemp_staged_cmd(Path::new(evil), "write").is_err(),
                "{evil:?}"
            );
        }
        // The prefix lands inside the template, so it must sanitize: no
        // separator, no dot-leading traversal, no control chars.
        for evil in ["../../escape", "/etc/passwd", "with space", "with\nnewline"] {
            assert!(
                mktemp_staged_cmd(dir, evil).is_err(),
                "prefix {evil:?} must be rejected"
            );
        }
        for evil in ["600", "0600x", "abc", ""] {
            assert!(chmod_staged_cmd(evil, dir).is_err(), "mode {evil:?}");
        }
        for evil in ["relative", "", "--reference=/etc/shadow"] {
            assert!(
                chown_one_cmd(1000, 1000, Path::new(evil)).is_err(),
                "{evil:?}"
            );
            assert!(mv_staged_cmd(dir, Path::new(evil)).is_err(), "{evil:?}");
            assert!(cp_staged_cmd(Path::new(evil), dir).is_err(), "{evil:?}");
        }
    }

    /// Injection matrix for the unshare payloads: hostile paths/mods/commands
    /// are rejected before any argv exists (ADR-0010 typed-source discipline).
    #[test]
    fn unshare_payload_injection_rejected() {
        // Command payload: empty, leading-dash binary, control chars.
        assert!(unshare_exec_argv(&bin(), &[]).is_err());
        assert!(unshare_exec_argv(&bin(), &["-rf".to_owned()]).is_err());
        assert!(unshare_exec_argv(&bin(), &["getfacl\n--privileged".to_owned()]).is_err());
        assert!(unshare_exec_argv(&bin(), &["".to_owned()]).is_err());
        // Paths: relative, empty, leading dash, newline.
        for evil in ["relative/path", "", "-m", "/v/has\nnewline"] {
            assert!(
                getfacl_cmd(Path::new(evil)).is_err(),
                "{evil:?} must be rejected"
            );
            assert!(
                cat_cmd(Path::new(evil)).is_err(),
                "{evil:?} must be rejected"
            );
            assert!(
                setfacl_cmd(&["u:1001:rwx".to_owned()], Path::new(evil)).is_err(),
                "{evil:?} must be rejected"
            );
        }
        // Traversal-shaped paths are blocked UPSTREAM (SafeName/tree
        // accessors, ADR-0002/0003): validate_host_path is positional and
        // stays opaque to `/../`, exactly like unshare_chown's validator.
        // setfacl mods: only u:<digits>:[r-][w-][x-] tokens pass.
        for evil in [
            "",
            "g:1001:rwx",
            "u:1001",
            "u:1001:rws",
            "u:-1:rwx",
            "u:1001:rwx -m u:0:rwx",
            "u:1001:rwxx",
            "x:1001:rwx",
            "u:1oo1:rwx",
        ] {
            assert!(
                setfacl_cmd(&[evil.to_owned()], Path::new("/v/.hamlet/x")).is_err(),
                "{evil:?} must be rejected"
            );
        }
        assert!(setfacl_cmd(&[], Path::new("/v/.hamlet/x")).is_err());
        assert!(setfacl_cmd(&["u:1001:rwx".to_owned()], Path::new("/v/.hamlet/x")).is_ok());
    }

    /// Verb-targeted failure injection: `fail_next_for` hits exactly the
    /// named podman subcommand, leaving earlier calls (e.g. `pull`'s probe)
    /// successful — the seam the provisioning rollback tests drive.
    #[tokio::test]
    async fn verb_targeted_failure_injection() {
        let fake = FakeRuntime::new();
        let spec = agent_spec();
        let t = tree();
        fake.fail_next_for("run", PodmanErrorKind::NonZeroExit { code: Some(125) });
        // pull (image probe) succeeds despite the queued failure…
        fake.pull("docker.io/library/alpine:3.20").await.unwrap();
        // …and the next `run` consumes it.
        let err = fake.run_detached(&spec, &t).await.unwrap_err();
        assert_eq!(err.kind, PodmanErrorKind::NonZeroExit { code: Some(125) });
        // Queue drained: the next run succeeds.
        assert!(fake.run_detached(&spec, &t).await.is_ok());
        // The generic queue still works alongside.
        fake.fail_next(PodmanErrorKind::Spawn("gone".to_owned()));
        assert!(fake.remove("scribe", true).await.is_err());
    }

    /// Flag-injection guard: container names are positional argv slots, so
    /// `--all` / `-a` style names must die at the sanitizer, never spawn.
    #[test]
    fn container_name_injection_rejected() {
        for evil in ["--all", "-a", "../x", "a/b", "", "x y", "--force;rm"] {
            assert!(
                kill_argv(&bin(), evil).is_err(),
                "{evil:?} must be rejected"
            );
            assert!(exec_argv(&bin(), evil, &cmd(&["true"]), false).is_err());
        }
    }

    #[test]
    fn command_and_image_validation() {
        assert!(validate_cmd(&[]).is_err());
        assert!(validate_cmd(&cmd(&["--help"])).is_err());
        assert!(validate_cmd(&cmd(&["ls", "a\0b"])).is_err());
        assert!(validate_cmd(&cmd(&["ls", "-la"])).is_ok());
        assert!(validate_image("").is_err());
        assert!(validate_image("-x").is_err());
        assert!(validate_image("a b").is_err());
        assert!(validate_image("docker.io/library/alpine:3.20").is_ok());
    }

    #[test]
    fn runspec_uid_policy() {
        for uid in [0u32, 999, 1000, 2001, u32::MAX] {
            // 1000 is the Mayor's — for_agent must still refuse it (the
            // Mayor path is for_mayor only).
            let err = RunSpec::for_agent(
                &sanitize_name("x").unwrap(),
                uid,
                1,
                &blueprint(),
                &config(),
            );
            assert!(err.is_err(), "for_agent must reject uid {uid}");
        }
        // Hand-assembled spec with a bad uid fails at argv build too.
        let mut spec = agent_spec();
        spec.uid = 4242;
        assert!(spec.to_run_argv(&tree(), &bin()).is_err());
    }

    #[test]
    fn state_parsing_golden() {
        assert_eq!(parse_container_state("running\n"), ContainerState::Running);
        assert_eq!(parse_container_state("exited"), ContainerState::Exited);
        assert_eq!(parse_container_state("paused"), ContainerState::Paused);
        assert_eq!(parse_container_state("created"), ContainerState::Unknown);
        assert_eq!(parse_container_state(""), ContainerState::Unknown);
    }

    /// Env values are masked in every error/log rendering; the argv
    /// skeleton stays auditable (ADR-0010 cl. 5).
    #[test]
    fn redaction_masks_env_values_only() {
        let argv = vec![
            "podman".to_owned(),
            "run".to_owned(),
            "-e".to_owned(),
            "CARGO_HOME=/var/pkgcache/cargo".to_owned(),
            "--env=LANG=C.UTF-8".to_owned(),
            "--memory".to_owned(),
            "512m".to_owned(),
        ];
        let redacted = redacted_argv(&argv);
        assert_eq!(
            redacted,
            vec![
                "podman",
                "run",
                "-e",
                "CARGO_HOME=<redacted>",
                "--env=LANG=<redacted>",
                "--memory",
                "512m",
            ]
        );
        // And through the error type's Display (the failure-path log).
        let err = PodmanError::exit(
            Some(1),
            vec![
                "podman".to_owned(),
                "run".to_owned(),
                "-e".to_owned(),
                "SECRETISH=hunter2".to_owned(),
            ],
            "boom".to_owned(),
        );
        let rendered = err.to_string();
        assert!(
            !rendered.contains("hunter2"),
            "env value leaked: {rendered}"
        );
        assert!(rendered.contains("SECRETISH=<redacted>"));
        assert!(rendered.contains("boom"), "stderr must appear verbatim");
        assert!(rendered.contains("podman run"));
    }

    /// The run argv itself carries env verbatim (podman needs it) — the
    /// *error copy* is what must be redacted.
    #[test]
    fn run_argv_env_pairs_sorted_and_typed() {
        let spec = agent_spec();
        let argv = spec.to_run_argv(&tree(), &bin()).unwrap();
        let env_keys: Vec<&str> = argv
            .iter()
            .zip(argv.iter().skip(1))
            .filter(|(a, _)| a.as_str() == "-e")
            .map(|(_, kv)| kv.split_once('=').unwrap().0)
            .collect();
        let mut sorted = env_keys.clone();
        sorted.sort_unstable();
        assert_eq!(env_keys, sorted, "BTreeMap order");
        assert!(!env_keys.is_empty(), "default blueprint ships [env]");
    }

    #[tokio::test]
    async fn fake_runtime_failure_injection() {
        let fake = FakeRuntime::new();
        let spec = agent_spec();
        let t = tree();
        let injected = PodmanErrorKind::NonZeroExit { code: Some(125) };

        fake.fail_next(injected.clone());
        let err = fake.run_detached(&spec, &t).await.unwrap_err();
        assert_eq!(err.kind, injected);
        assert!(err.argv.iter().any(|a| a == "run"));
        assert_eq!(err.stderr, "fake: injected failure");

        fake.fail_next(PodmanErrorKind::NoSuchContainer {
            name: "scribe".to_owned(),
        });
        assert!(matches!(
            fake.stop("scribe", Duration::from_secs(5)).await,
            Err(PodmanError {
                kind: PodmanErrorKind::NoSuchContainer { .. },
                ..
            })
        ));

        fake.fail_next(PodmanErrorKind::Spawn("no podman".to_owned()));
        assert!(fake.kill("scribe").await.is_err());
        fake.fail_next(PodmanErrorKind::Spawn("no podman".to_owned()));
        assert!(fake.state("scribe").await.is_err());
        fake.fail_next(PodmanErrorKind::Spawn("no podman".to_owned()));
        assert!(fake.exec("scribe", &cmd(&["true"]), None).await.is_err());
        // Failures are consumed one-per-call; the next call succeeds.
        assert!(fake.kill("scribe").await.is_ok());
    }

    /// Pre-flight validation errors never record an argv (nothing spawned).
    #[tokio::test]
    async fn invalid_specs_spawn_nothing() {
        let fake = FakeRuntime::new();
        assert!(fake.kill("--all").await.is_err());
        assert!(fake.exec("scribe", &cmd(&[]), None).await.is_err());
        assert!(fake.pull("").await.is_err());
        assert!(fake.argv_log().is_empty());
    }

    #[tokio::test]
    async fn pull_is_idempotent() {
        let fake = FakeRuntime::new();
        fake.pull("alpine:3.20").await.unwrap();
        let log = fake.argv_log();
        assert_eq!(log.len(), 1, "exists probe only");
        assert_eq!(log[0], vec!["podman", "image", "exists", "alpine:3.20"]);

        fake.set_image_exists(false);
        fake.pull("alpine:3.20").await.unwrap();
        let log = fake.argv_log();
        assert_eq!(log.len(), 3);
        assert_eq!(log[2], vec!["podman", "pull", "alpine:3.20"]);
    }

    #[tokio::test]
    async fn scripted_state_and_exec_flow_through_the_trait() {
        let fake = FakeRuntime::new();
        fake.set_state(ContainerState::Exited);
        assert_eq!(fake.state("scribe").await.unwrap(), ContainerState::Exited);
        fake.set_exec_output(ExecOutput {
            code: 0,
            stdout: b"1001\n".to_vec(),
            stderr: Vec::new(),
        });
        let out = fake
            .exec("scribe", &cmd(&["id", "-u"]), None)
            .await
            .unwrap();
        assert_eq!(out.stdout_lossy().trim(), "1001");
    }

    #[tokio::test]
    async fn fake_exec_handle_pipes_stdio() {
        let fake = FakeRuntime::new();
        fake.set_exec_handle(b"hello stdout".to_vec(), 3);
        let mut handle = fake.spawn_exec("scribe", &cmd(&["pi"])).await.unwrap();
        let mut pipes = handle.take_pipes().expect("pipes detach exactly once");
        assert!(handle.take_pipes().is_none(), "second take must be None");
        pipes
            .stdin
            .write_all(b"{\"jsonrpc\":\"2.0\"}\n")
            .await
            .unwrap();
        let mut buf = String::new();
        pipes.stdout.read_to_string(&mut buf).await.unwrap();
        assert_eq!(buf, "hello stdout");
        assert_eq!(handle.wait().await.unwrap(), 3);
        // The spawn was recorded as an interactive exec.
        assert_eq!(
            fake.argv_log().pop().unwrap(),
            vec!["podman", "exec", "-i", "scribe", "pi"]
        );
    }

    #[tokio::test]
    async fn fake_exec_handle_kill_reports_signal_exit() {
        let fake = FakeRuntime::new();
        fake.set_exec_handle(Vec::new(), 0);
        let mut handle = fake.spawn_exec("scribe", &cmd(&["pi"])).await.unwrap();
        handle.kill().unwrap();
        assert_eq!(handle.wait().await.unwrap(), SIGNAL_EXIT_CODE);
    }

    /// Golden: the ADR-0015 cl. 1 session argv — sorted `-e` flags,
    /// `--user`/`--workdir` from typed sources, values masked by redaction.
    #[test]
    fn session_exec_argv_golden() {
        let mut env = BTreeMap::new();
        env.insert("PI_TELEMETRY".to_owned(), "0".to_owned());
        env.insert("ANTHROPIC_API_KEY".to_owned(), "sk-secret".to_owned());
        let spec = SessionExecSpec {
            container: "scribe".to_owned(),
            uid: 1001,
            env,
            cmd: cmd(&["pi", "--mode", "rpc", "--continue"]),
        };
        let argv = session_exec_argv(Path::new("podman"), &spec).unwrap();
        assert_eq!(
            argv,
            vec![
                "podman",
                "exec",
                "-i",
                "--user",
                "1001:1001",
                "--workdir",
                "/home/1001",
                "-e",
                "ANTHROPIC_API_KEY=sk-secret",
                "-e",
                "PI_TELEMETRY=0",
                "scribe",
                "pi",
                "--mode",
                "rpc",
                "--continue",
            ]
        );
        // Secrets never reach logs: the redacted form masks every value.
        assert_eq!(
            redacted_argv(&argv)[7..11],
            [
                "-e",
                "ANTHROPIC_API_KEY=<redacted>",
                "-e",
                "PI_TELEMETRY=<redacted>"
            ]
        );
        // Mayor sessions run as uid 1000 in the manor home.
        let mayor = SessionExecSpec {
            uid: MAYOR_UID,
            container: "manor".to_owned(),
            ..spec.clone()
        };
        let argv = session_exec_argv(Path::new("podman"), &mayor).unwrap();
        assert_eq!(
            argv[3..7],
            ["--user", "1000:1000", "--workdir", "/home/1000"]
        );
    }

    #[test]
    fn session_exec_argv_rejects_bad_inputs() {
        let base = SessionExecSpec {
            container: "scribe".to_owned(),
            uid: 1001,
            env: BTreeMap::new(),
            cmd: cmd(&["pi", "--mode", "rpc"]),
        };
        let cases: Vec<(SessionExecSpec, &str)> = vec![
            (
                SessionExecSpec {
                    uid: 999,
                    ..base.clone()
                },
                "uid outside slot policy",
            ),
            (
                SessionExecSpec {
                    uid: 2001,
                    ..base.clone()
                },
                "uid outside slot policy",
            ),
            (
                SessionExecSpec {
                    container: "--all".to_owned(),
                    ..base.clone()
                },
                "container flag injection",
            ),
            (
                SessionExecSpec {
                    container: "a/b".to_owned(),
                    ..base.clone()
                },
                "container separator",
            ),
            (
                SessionExecSpec {
                    cmd: vec!["-pi".to_owned()],
                    ..base.clone()
                },
                "leading-dash command",
            ),
            (
                SessionExecSpec {
                    cmd: Vec::new(),
                    ..base.clone()
                },
                "empty command",
            ),
            (
                SessionExecSpec {
                    env: [("BAD KEY".to_owned(), "v".to_owned())]
                        .into_iter()
                        .collect(),
                    ..base.clone()
                },
                "env key charset",
            ),
            (
                SessionExecSpec {
                    env: [("K".to_owned(), "a\nb".to_owned())].into_iter().collect(),
                    ..base
                },
                "env value control chars",
            ),
        ];
        for (spec, why) in cases {
            assert!(
                session_exec_argv(Path::new("podman"), &spec).is_err(),
                "must reject: {why}"
            );
        }
    }

    #[tokio::test]
    async fn fake_spawn_session_records_redacted_argv() {
        let fake = FakeRuntime::new();
        let mut env = BTreeMap::new();
        env.insert(
            "HAMLET_MODEL_API_KEY".to_owned(),
            "test-key-do-not-use".to_owned(),
        );
        let spec = SessionExecSpec {
            container: "scribe".to_owned(),
            uid: 1042,
            env,
            cmd: cmd(&["pi", "--mode", "rpc"]),
        };
        let _handle = fake.spawn_session(&spec).await.unwrap();
        let argv = fake.argv_log().pop().unwrap();
        assert!(argv.contains(&"HAMLET_MODEL_API_KEY=<redacted>".to_owned()));
        assert!(!argv.join(" ").contains("test-key-do-not-use"));
        // Failure injection works on the session verb too.
        fake.fail_next_for(
            "exec",
            PodmanErrorKind::NoSuchContainer {
                name: "x".to_owned(),
            },
        );
        assert!(matches!(
            fake.spawn_session(&spec).await,
            Err(e) if matches!(e.kind, PodmanErrorKind::NoSuchContainer { .. })
        ));
    }

    #[test]
    fn resource_limits_reject_tampered_values() {
        let bad = [
            ResourceLimits {
                memory: "--privileged".to_owned(),
                cpus: "1".to_owned(),
                pids_limit: 10,
            },
            ResourceLimits {
                memory: "512m".to_owned(),
                cpus: "1;rm -rf /".to_owned(),
                pids_limit: 10,
            },
            ResourceLimits {
                memory: "512m".to_owned(),
                cpus: "1".to_owned(),
                pids_limit: 0,
            },
        ];
        for limits in bad {
            assert!(limits.validate().is_err(), "{limits:?} must be rejected");
        }
        assert!(ResourceLimits::from(&config()).validate().is_ok());
    }
}
