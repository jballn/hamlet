//! The Mayor loop — session lifecycle half (SPEC §7; ADR-0028).
//!
//! The Mayor is a **stateless event processor**, not a chat: it sleeps at
//! zero token cost, is woken by an event, receives one pristine
//! single-turn [`ContextPacket`] (MAYOR.md + charter state grid + the
//! targeted event artifact — ADR-0027), evaluates once, and sleeps again.
//! This module owns the *lifecycle*; step 03 adds decree interpretation on
//! top of the same seams.
//!
//! # Why a fresh pi invocation per wake (ADR-0028 cl. 1)
//!
//! The two candidate strategies were a **parked session** (one long-lived
//! pi process, one fresh turn per wake) and a **fresh invocation** (one pi
//! process per wake, exited immediately after the task boundary). ADR-0015
//! records the decisive finding: `--continue` *resumes pi's own session
//! history* from the parcel home. A parked process therefore accumulates a
//! thread across wakes — precisely what SPEC §7 forbids ("no continuous
//! chat thread is ever maintained") — and the only way to prune it would be
//! to delete or reset state inside the Mayor's own home, i.e. the daemon
//! mutating the agent's private context behind its back. So:
//!
//! - **one wake = one process = one prompt = one turn**, then stdin EOF and
//!   exit ([`MayorEngine::evaluate`]);
//! - **between wakes: no process at all**, so "zero token cost" is a
//!   structural property, not a hope ([`SleepState`]);
//! - **no `--continue`, ever** — the argv comes from
//!   [`pi_session_cmd`](crate::supervise::pi_session_cmd), which never adds
//!   it, and a restart-policy restart is refused rather than resumed.
//!
//! # Zero-token sleep, asserted
//!
//! [`MayorEngine`] owns no session between calls: the spawned process is
//! awaited to exit inside `evaluate`. [`SleepState::Awake`] is false after
//! every wake, and no stdout byte can arrive when nothing is running. The
//! integration suite asserts both (packet golden + "log gains no lines").
//!
//! # Wake serialization and merging
//!
//! [`WakeQueue`] is bounded (`config.mayor.wake_queue_depth`) and merges:
//! events that share a wake reason accumulate into one evaluation carrying
//! all of them ([`hamlet_core::packet::EventArtifact::merged`]) — five mails
//! are one wake with five artifacts, not five turns. Merging is FIFO- and
//! size-deterministic, and a batch that would exceed the artifact cap splits
//! across further wakes instead of truncating a prompt.
//!
//! [`MayorEngine::wake`] serializes evaluations on an async mutex: one
//! evaluation at a time, no matter how many watchers fire. A second event
//! arriving during an evaluation is queued and drained by the holder (or by
//! the next waiter) — never evaluated concurrently.
//!
//! # File-as-truth
//!
//! Nothing here is daemon-only state: the queued events are re-derived from
//! the tree by the watch plane (step 03), the packet is a pure function of
//! MAYOR.md + charter + artifact, and the turn's lifecycle is written to
//! `charter.json` (`sessions[1000]`, one incarnation per wake) so a killed
//! daemon leaves an observable trail that [`MayorEngine::recover`] heals.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{AgentRef, Charter, CharterError};
use hamlet_core::config::Config;
use hamlet_core::decree::{DecreeId, TreeRelPath};
use hamlet_core::dispatch::{
    file_name as inbox_file_name, InboxResponse, Outcome as DispatchOutcome,
};
use hamlet_core::mail::Mail;
use hamlet_core::packet::{
    BatchMember, ContextPacket, EventArtifact, EventKind, PacketError, StateGrid, WakeReason,
};
use hamlet_core::sanitize::{sanitize_name, SafeName, SanitizeError};
use hamlet_core::session::{
    parse_agent_event, pi_binding, readiness_probe, AgentEvent, CorrelationId, QueuedKind,
    SessionEnvelope, SessionError, SessionId, SessionRecord, SessionStatus, Usage,
};
use hamlet_core::slots::{SlotError, MAYOR_UID};
use hamlet_core::tree::HamletTree;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{timeout, Duration as TokioDuration};

use crate::bootstrap::{BootstrapError, Bootstrapper, ImageOutcome};
use crate::freight::{FreightEscalation, MayorWake};
use crate::hostperm::{set_mode, HostPermError, HostPermissions, MODE_PARCEL_DIR, MODE_PARCEL_DOC};
use crate::podman::{PodmanError, RunSpec, Runtime, SessionExecSpec, SIGNAL_EXIT_CODE};
use crate::supervise::{pi_session_cmd, scrub_detail, CrashNotice, SessionSpawner};
use crate::telemetry::{AgentKey, Telemetry, TraceRecord};
use crate::web::{WebhookEvent, WebhookWake};

/// The committed v1 trunk prompt, seeded into `keep/census/manor/MAYOR.md`
/// the first time the manor is provisioned. Provisioning **adopts** an
/// existing file and never clobbers it: the Mayor's behavior prompt is
/// operator-owned once written (SPEC §2 trunk prompt; CONVENTIONS §2).
pub const MAYOR_MD_TEMPLATE: &str = include_str!("../../../defaults/MAYOR.md");

/// The reserved Mayor identity (`charter.reserve_mayor`, SPEC §4 UID 1000).
pub const MAYOR_IDENTITY: &str = "manor";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed Mayor-lifecycle failures. Every variant is a refusal — the daemon
/// never degrades a packet, a wake, or a turn into a guess.
#[derive(Debug)]
pub enum MayorError {
    /// Charter load/validate/save failure.
    Charter(CharterError),
    /// Slot policy failure (UID 1000 is permanent).
    Slot(SlotError),
    /// Podman/exec failure.
    Podman(PodmanError),
    /// Host permission failure (ADR-0011).
    HostPerm(HostPermError),
    /// Blueprint image resolution failure (ADR-0012).
    Bootstrap(BootstrapError),
    /// Packet schema refusal (caps, empty MAYOR.md, mixed-reason merge).
    Packet(PacketError),
    /// Session protocol failure (identity, binding).
    Session(SessionError),
    /// Mail artifact parse failure.
    Mail(hamlet_core::mail::MailError),
    /// Sanitizer refusal on a derived name (CONVENTIONS §4).
    Sanitize(SanitizeError),
    /// Filesystem failure with the step that failed.
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// A path that must be a regular file is a symlink/dir/other — refused,
    /// never followed or adopted (ADR-0007).
    NotRegularFile {
        /// The offending path.
        path: PathBuf,
    },
    /// `keep/census/manor/MAYOR.md` is absent — the packet cannot be built
    /// (an empty prompt is a corrupted one, ADR-0027).
    MissingMayorMd {
        /// Expected path.
        path: PathBuf,
    },
    /// The spawned process carried no pipes.
    NoPipes,
    /// pi refused the readiness probe.
    ReadinessRefused,
    /// The single turn exceeded `mayor.turn_timeout_ms` and was abandoned.
    TurnTimeout {
        /// The bound that elapsed, milliseconds.
        ms: u64,
    },
    /// The session process did not exit inside the stop-grace window.
    ExitTimeout,
    /// The charter holds no slot for UID 1000 — the manor must be
    /// provisioned before the Mayor can be woken (a boot-order bug, never
    /// papered over by allocating a slot on the fly).
    NoMayorSlot,
    /// A village-relative path failed validation (ADR-0026 component
    /// allowlist).
    TreeRel(hamlet_core::decree::DecreeError),
    /// The exec handle produced no usable exit code.
    ExitFailed(PodmanError),
}

impl fmt::Display for MayorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Charter(e) => write!(f, "mayor: charter: {e}"),
            Self::Slot(e) => write!(f, "mayor: slot: {e}"),
            Self::Podman(e) => write!(f, "mayor: {e}"),
            Self::HostPerm(e) => write!(f, "mayor: {e}"),
            Self::Bootstrap(e) => write!(f, "mayor: {e}"),
            Self::Packet(e) => write!(f, "mayor: packet: {e}"),
            Self::Session(e) => write!(f, "mayor: session: {e}"),
            Self::Mail(e) => write!(f, "mayor: mail: {e}"),
            Self::Sanitize(e) => write!(f, "mayor: sanitize: {e}"),
            Self::Io { context, source } => write!(f, "mayor: {context}: {source}"),
            Self::NotRegularFile { path } => write!(
                f,
                "mayor: {} must be a regular file; refusing to follow or adopt (ADR-0007)",
                path.display()
            ),
            Self::MissingMayorMd { path } => write!(
                f,
                "mayor: no trunk prompt at {} — the packet cannot be built",
                path.display()
            ),
            Self::NoPipes => write!(f, "mayor: exec handle carried no stdio pipes"),
            Self::ReadinessRefused => write!(f, "mayor: pi refused the readiness probe"),
            Self::TurnTimeout { ms } => {
                write!(f, "mayor: single turn exceeded {ms}ms; abandoned")
            }
            Self::ExitTimeout => write!(f, "mayor: session did not exit within the grace window"),
            Self::ExitFailed(e) => write!(f, "mayor: session exit: {e}"),
            Self::NoMayorSlot => write!(
                f,
                "mayor: no charter slot for uid {MAYOR_UID}; provision the manor first"
            ),
            Self::TreeRel(e) => write!(f, "mayor: village-relative path: {e}"),
        }
    }
}

impl std::error::Error for MayorError {}

impl From<SlotError> for MayorError {
    fn from(e: SlotError) -> Self {
        Self::Slot(e)
    }
}

impl From<std::io::Error> for MayorError {
    fn from(e: std::io::Error) -> Self {
        Self::Io {
            context: "mayor io".to_owned(),
            source: e,
        }
    }
}

impl From<CharterError> for MayorError {
    fn from(e: CharterError) -> Self {
        Self::Charter(e)
    }
}
impl From<PodmanError> for MayorError {
    fn from(e: PodmanError) -> Self {
        Self::Podman(e)
    }
}
impl From<HostPermError> for MayorError {
    fn from(e: HostPermError) -> Self {
        Self::HostPerm(e)
    }
}
impl From<BootstrapError> for MayorError {
    fn from(e: BootstrapError) -> Self {
        Self::Bootstrap(e)
    }
}
impl From<PacketError> for MayorError {
    fn from(e: PacketError) -> Self {
        Self::Packet(e)
    }
}
impl From<SessionError> for MayorError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}
impl From<hamlet_core::mail::MailError> for MayorError {
    fn from(e: hamlet_core::mail::MailError) -> Self {
        Self::Mail(e)
    }
}
impl From<hamlet_core::decree::DecreeError> for MayorError {
    fn from(e: hamlet_core::decree::DecreeError) -> Self {
        Self::TreeRel(e)
    }
}
impl From<SanitizeError> for MayorError {
    fn from(e: SanitizeError) -> Self {
        Self::Sanitize(e)
    }
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> MayorError {
    move |source| MayorError::Io {
        context: context.into(),
        source,
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}

/// The Mayor's reserved charter identity, sanitized (CONVENTIONS §4).
///
/// # Errors
///
/// [`SanitizeError`] — only if the committed constant is edited into an
/// invalid name (a build-time bug, kept typed rather than unwrapped).
pub fn mayor_identity() -> Result<SafeName, SanitizeError> {
    sanitize_name(MAYOR_IDENTITY)
}

// ---------------------------------------------------------------------------
// Manor provisioning (SPEC §7)
// ---------------------------------------------------------------------------

/// What one manor provisioning run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManorProvisioned {
    /// The Mayor's permanent charter slot (always [`MAYOR_UID`]).
    pub uid: u32,
    /// The derived image the manor container runs (ADR-0012 cl. 2).
    pub image: ImageOutcome,
    /// False when an existing regular `MAYOR.md` was adopted (never
    /// clobbered — the operator owns the trunk prompt once written).
    pub seeded_mayor_md: bool,
}

/// The manor governor: reserves UID 1000, creates the parcel-shaped manor
/// (`keep/census/manor/{MAYOR.md,home/}`), hardens it under ADR-0011, and
/// starts the Mayor container with [`MountSet::for_mayor`] (SPEC §4) via
/// the [`Runtime`] seam.
///
/// **Crash convergence:** re-provisioning adopts intact state — the charter
/// slot is idempotent per identity, `MAYOR.md` is adopted when it already
/// exists, and host modes are re-asserted. A live container from a previous
/// run makes `run_detached` fail on the taken name; that error is surfaced
/// rather than papered over (rollback never removes containers it did not
/// start, SPEC §6).
pub struct ManorProvisioner {
    tree: HamletTree,
    config: Config,
    runtime: Arc<dyn Runtime>,
}

impl ManorProvisioner {
    /// Provisioner over a tree, the engine config, and the runtime seam.
    #[must_use]
    pub fn new(tree: HamletTree, config: Config, runtime: Arc<dyn Runtime>) -> Self {
        Self {
            tree,
            config,
            runtime,
        }
    }

    /// The village tree this provisioner governs.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Provision (or re-converge) the manor for the Mayor.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — charter save, io, host-permission, image or podman
    /// failures. Side effects are ordered so the charter is written **last**
    /// (CONVENTIONS §3 r2): a failed run never leaves a recorded lease whose
    /// container failed to start.
    pub async fn provision(&self, blueprint: &Blueprint) -> Result<ManorProvisioned, MayorError> {
        let identity = mayor_identity()?;

        // --- 1. permanent slot for the trunk agent (idempotent) ----------
        let mut charter = Charter::load(&self.tree.charter())?;
        let uid = charter.reserve_mayor(identity.clone())?;
        debug_assert_eq!(uid, MAYOR_UID, "the Mayor is UID 1000 by SPEC §4");

        // --- 2. manor shape: MAYOR.md, home/, home/decrees/ -------------
        let manor = self.tree.manor();
        let home = self.tree.manor_home();
        let decrees = self.tree.manor_decrees();
        for dir in [&manor, &home, &decrees] {
            fs::create_dir_all(dir).map_err(io_err(format!("create {}", dir.display())))?;
        }
        set_mode(&manor, MODE_PARCEL_DIR)?;
        // The Mayor's mail spool is provisioned with the manor (ADR-0011
        // cl. 2 owns it by uid 1000 before any container can reach it).
        let spool_name = sanitize_name(&uid.to_string())?;
        let spool = self.tree.courier_mail_recipient(&spool_name);
        fs::create_dir_all(&spool).map_err(io_err(format!("create {}", spool.display())))?;
        let seeded = seed_mayor_md(&self.tree)?;
        if !seeded {
            set_mode(&self.tree.manor_mayor_md(), MODE_PARCEL_DOC)?;
        }

        // --- 3. host permission braces (ADR-0011) ------------------------
        let perms = HostPermissions::new(self.runtime.as_ref());
        perms.apply_manor_home(&self.tree, uid).await?;
        perms.apply_mail_spool(&self.tree, uid).await?;

        // --- 4. derived image + container (SPEC §4 mount table) ----------
        let bootstrapper = Bootstrapper::new(&self.tree, self.runtime.as_ref())?;
        let image = bootstrapper.ensure_image(blueprint).await?;
        let spec =
            RunSpec::for_mayor(&identity, blueprint, &self.config)?.with_image(image.tag.clone());
        let _container_id = self.runtime.run_detached(&spec, &self.tree).await?;

        // --- 5. record the reservation (atomic; last) ---------------------
        charter.validate()?;
        charter.save_atomic(&self.tree.charter())?;
        Ok(ManorProvisioned {
            uid,
            image,
            seeded_mayor_md: seeded,
        })
    }
}

/// Seed `keep/census/manor/MAYOR.md` from the committed template.
///
/// Returns `true` when this call wrote it, `false` when an existing regular
/// file was adopted. Absent → `create_new` (never follows a planted symlink);
/// regular file → adopted untouched (crash convergence, operator edits);
/// anything else → [`MayorError::NotRegularFile`].
///
/// # Errors
///
/// [`MayorError`] on io failures or a non-regular path.
fn seed_mayor_md(tree: &HamletTree) -> Result<bool, MayorError> {
    let path = tree.manor_mayor_md();
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => Ok(false),
        Ok(_) => Err(MayorError::NotRegularFile { path }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(io_err(format!("create {}", path.display())))?;
            file.write_all(MAYOR_MD_TEMPLATE.as_bytes())
                .map_err(io_err(format!("write {}", path.display())))?;
            Ok(true)
        }
        Err(source) => Err(MayorError::Io {
            context: format!("stat {}", path.display()),
            source,
        }),
    }
}

// ---------------------------------------------------------------------------
// Wake events
// ---------------------------------------------------------------------------

/// One typed wake input: what happened, plus the verbatim artifact text the
/// Mayor must read. Built through the constructors, which load from the
/// tree or accept already-typed values; the artifact cap is enforced at
/// construction (ADR-0027: refusals, never truncations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeEvent {
    artifact: EventArtifact,
    source: WakeSource,
}

/// **Where an event came from**, as a stable identity the queue dedupes on
/// (QA05-06).
///
/// Dedupe used to compare rendered `(kind, body)` pairs, which conflates two
/// genuinely different events that happen to serialize identically — the same
/// sandbox quarantined twice for the same owner, two identical crash lines —
/// into one wake, so the Mayor was never told the second thing happened.
/// Equality of *content* is not equality of *events*.
///
/// What identity is stable depends on the plane, and each constructor knows
/// its own: a spool path for mail, a correlation for a webhook, a decree id +
/// notice number for an ack timeout, `uid/session/restarts` for a crash. The
/// value is queue bookkeeping only — it never reaches the packet, so it is
/// telemetry-grade, not state (CONVENTIONS §3 r4).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WakeSource(String);

impl WakeSource {
    fn new(kind: &'static str, identity: impl std::fmt::Display) -> Self {
        Self(format!("{kind}:{identity}"))
    }

    /// The stable `plane:identity` key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WakeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl WakeEvent {
    /// A wake from an already-typed kind + body.
    ///
    /// # Errors
    ///
    /// [`PacketError`] — empty/blank or oversize body.
    pub fn new(kind: EventKind, body: impl Into<String>) -> Result<Self, MayorError> {
        let body = body.into();
        let source = Self::adhoc_source(&kind, &body);
        Ok(Self {
            artifact: EventArtifact::new(kind, body)?,
            source,
        })
    }

    /// A wake whose source identity the caller supplies, for the planes
    /// where the constructor does not know it.
    ///
    /// # Errors
    ///
    /// [`PacketError`] — empty/blank or oversize body.
    pub fn with_source(
        kind: EventKind,
        body: impl Into<String>,
        source: WakeSource,
    ) -> Result<Self, MayorError> {
        Ok(Self {
            artifact: EventArtifact::new(kind, body)?,
            source,
        })
    }

    /// The stable source identity this event dedupes on (QA05-06).
    #[must_use]
    pub fn source(&self) -> &WakeSource {
        &self.source
    }

    /// Inbound mail for the Mayor's slot (SPEC §8.1): read the spool file,
    /// parse it as ADR-0019 mail, and hand the Mayor the canonical message.
    /// The event's typed header is what the daemon validated; the body is
    /// the message verbatim.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — io failure, mail parse failure, or cap refusal.
    pub fn from_mail_spool(tree: &HamletTree, spool: &std::path::Path) -> Result<Self, MayorError> {
        let raw = fs::read_to_string(spool)
            .map_err(io_err(format!("read mail spool {}", spool.display())))?;
        let mail = Mail::parse(&raw)?;
        let rel = village_relative(tree, spool)?;
        let kind = EventKind::MailReceived {
            from: mail.header.from.clone(),
            subject: mail.header.subject.clone(),
            spool: TreeRelPath::parse(&rel)?,
        };
        Ok(Self {
            artifact: EventArtifact::new(kind, mail.render())?,
            // The spool file is the event: two deliveries of the same text
            // are two mails and must both reach the Mayor (QA05-06).
            source: WakeSource::new("mail", rel),
        })
    }

    /// An accepted inbound webhook (ADR-0022 cl. 4/6): typed topic,
    /// correlation and the `status: 202` accepted-marker, with the
    /// canonical event JSON as the body.
    ///
    /// # Errors
    ///
    /// [`PacketError`] — cap refusal on the event body.
    pub fn from_webhook(
        topic: SafeName,
        correlation: CorrelationId,
        status: u16,
        body: impl Into<String>,
    ) -> Result<Self, MayorError> {
        Ok(Self {
            artifact: EventArtifact::new(
                EventKind::WebhookEvent {
                    topic,
                    correlation: correlation.clone(),
                    status,
                },
                body,
            )?,
            source: WakeSource::new("webhook", correlation.to_string()),
        })
    }

    /// The webhook artifact loaded from the daemon's own inbox envelope
    /// (`courier/dispatch/inbox/<correlation>.json`): the file the tree
    /// actually holds, not a re-serialized guess.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — io/dispatch parse failure, an envelope that is not
    /// an HTTP outcome, or a cap refusal.
    pub fn from_webhook_inbox(
        tree: &HamletTree,
        topic: SafeName,
        correlation: &CorrelationId,
    ) -> Result<Self, MayorError> {
        let path = tree
            .courier_dispatch_inbox()
            .join(inbox_file_name(correlation));
        let raw =
            fs::read_to_string(&path).map_err(io_err(format!("read inbox {}", path.display())))?;
        let response = InboxResponse::parse(&raw).map_err(|e| MayorError::Io {
            context: format!("parse inbox {}", path.display()),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        })?;
        let DispatchOutcome::Http { status, body, .. } = response.outcome else {
            return Err(MayorError::Io {
                context: format!("inbox {}", path.display()),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "inbound webhook envelope is not an HTTP outcome",
                ),
            });
        };
        Self::from_webhook(topic, correlation.clone(), status, body)
    }

    /// A freight-audit escalation (ADR-0021 cl. 5): the offending sandbox
    /// and its owner, with the quarantine listing as the body. The audit's
    /// typed violation kinds are not carried here (step 03 refines the
    /// escalation payload); the body lists exactly what was quarantined.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — sanitizer refusal on a quarantine name or a cap
    /// refusal on the rendered listing.
    pub fn from_freight_escalation(escalation: &FreightEscalation) -> Result<Self, MayorError> {
        let sandbox = escalation
            .quarantined
            .first()
            .ok_or(MayorError::Packet(PacketError::EmptyArtifact))?;
        let sandbox = sanitize_name(sandbox)?;
        let mut body = format!("owner: {}\n", escalation.owner);
        for entry in &escalation.quarantined {
            body.push_str(&format!("quarantined: {entry}\n"));
        }
        Ok(Self {
            artifact: EventArtifact::new(
                EventKind::FreightViolation {
                    sandbox: sandbox.clone(),
                    owner: escalation.owner,
                    kinds: Vec::new(),
                },
                body,
            )?,
            // A freight escalation carries no audit id, so the identity is
            // the *occurrence*: who, what, and the index of this sandbox in
            // the audit listing. Two audits of the same violation have
            // different listings and both wake; the same audit reported twice
            // is one event (QA05-06).
            source: WakeSource::new(
                "freight",
                format!("{}/{}", escalation.owner, escalation.audit_id),
            ),
        })
    }

    /// A decree that passed `mayor.ack_timeout_ms` without its ack file
    /// (ADR-0026 cl. 5).
    ///
    /// # Errors
    ///
    /// [`PacketError`] — cap refusal.
    pub fn from_decree_ack_timeout(
        decree_id: DecreeId,
        waited_ms: u64,
        notice: u64,
    ) -> Result<Self, MayorError> {
        Ok(Self {
            artifact: EventArtifact::new(
                EventKind::DecreeAckTimeout {
                    decree_id: decree_id.clone(),
                    waited_ms,
                },
                format!(
                    "decree {decree_id} has no .ack file after {waited_ms}ms; \
                     the daemon is still waiting for its disposition\n\
                     timeout notice {notice} of this process \
                     (re-notified every mayor.ack_retry_ms)\n"
                ),
            )?,
            // Notice *n* of the same decree is a new fact the Mayor has not
            // been told: distinct events, distinct wakes (QA05-06).
            source: WakeSource::new("ack-timeout", format!("{decree_id}#{notice}")),
        })
    }

    /// A supervised agent session crashed (SPEC §6; ADR-0029 cl. 2). The
    /// body is the daemon-side crash line — no stderr, no secrets
    /// ([`scrub_detail`](crate::supervise::scrub_detail) already applied).
    ///
    /// # Errors
    ///
    /// [`PacketError`] — cap refusal.
    pub fn from_session_crash(notice: &CrashNotice) -> Result<Self, MayorError> {
        let body = format!(
            "uid: {}\nsession: {}\nrestarts: {}\npermanent: {}\nreason: {}\n",
            notice.uid, notice.session, notice.restarts, notice.permanent, notice.detail,
        );
        Ok(Self {
            artifact: EventArtifact::new(
                EventKind::SessionCrashed {
                    uid: notice.uid,
                    session: notice.session,
                    restarts: notice.restarts,
                    permanent: notice.permanent,
                },
                body,
            )?,
            // Session + restart count is the crash's identity: a *second*
            // crash of the same life (restarts incremented) is a new event,
            // while the supervisor reporting the same one twice is a double
            // fire (QA05-06).
            source: WakeSource::new(
                "crash",
                format!("{}/{}/{}", notice.uid, notice.session, notice.restarts),
            ),
        })
    }

    /// A new commission with its `COMMISSION.md` loaded from the ledger.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — io failure or cap refusal.
    pub fn from_commission(tree: &HamletTree, commission: u64) -> Result<Self, MayorError> {
        let path = tree.commission_md(commission);
        let body = fs::read_to_string(&path).map_err(io_err(format!("read {}", path.display())))?;
        Ok(Self {
            artifact: EventArtifact::new(EventKind::CommissionCreated { commission }, body)?,
            source: WakeSource::new("commission", commission),
        })
    }

    /// A work-branch diff ready for the validation loop (SPEC §7).
    ///
    /// # Errors
    ///
    /// [`PacketError`] — cap refusal.
    pub fn from_git_diff(
        commission: u64,
        base: SafeName,
        head: SafeName,
        diff: impl Into<String>,
    ) -> Result<Self, MayorError> {
        Ok(Self {
            artifact: EventArtifact::new(
                EventKind::GitDiffReady {
                    commission,
                    base: base.clone(),
                    head: head.clone(),
                },
                diff,
            )?,
            source: WakeSource::new("git", format!("{commission}/{base}/{head}")),
        })
    }

    /// The typed event kind.
    #[must_use]
    pub fn kind(&self) -> &EventKind {
        &self.artifact.kind
    }

    /// The wake-plane identity of an ad-hoc event: the event kind's token plus
    /// the kind's typed header block (which carries whatever that plane
    /// considers identity — spool path, correlation, decree id, uid) and a
    /// digest of the body. The header, not just the body, is what separates
    /// two events that happen to serialize alike.
    fn adhoc_source(kind: &EventKind, body: &str) -> WakeSource {
        let header = kind.header_lines().join("\n");
        WakeSource::new(
            kind.wake_reason().token(),
            format!("{header}\n{}", digest_key(body)),
        )
    }

    /// The verbatim artifact body.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.artifact.body
    }

    /// The derived wake reason.
    #[must_use]
    pub fn wake_reason(&self) -> WakeReason {
        self.artifact.kind.wake_reason()
    }

    /// The validated artifact (the only thing a packet may embed).
    #[must_use]
    pub fn artifact(&self) -> &EventArtifact {
        &self.artifact
    }
}

/// A short, stable key for an event that has no natural identity of its own
/// (`WakeEvent::new`, called directly by a plane): 16 hex chars of FNV-1a/64
/// over the body. Same dependency-free digest the decree identity uses
/// (ADR-0026 cl. 3) — a collision detector, not a security primitive.
fn digest_key(body: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in body.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Village-root-relative, `/`-separated path of `path` under `tree`'s root.
/// Typed-path policy (ADR-0003): nothing else constructs village paths.
fn village_relative(tree: &HamletTree, path: &std::path::Path) -> Result<String, MayorError> {
    let root = tree.root();
    let rel = path.strip_prefix(root).map_err(|_| MayorError::Io {
        context: format!("{} is outside the village root", path.display()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "outside root"),
    })?;
    let mut out = String::new();
    for component in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The bounded, merging wake queue
// ---------------------------------------------------------------------------

/// One event in the queue: a monotonic arrival seq plus its artifact. The
/// seq is telemetry/lineage only (CONVENTIONS §3 r4) — it never reaches the
/// packet as state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedWake {
    /// Monotonic arrival sequence within this process.
    pub seq: u64,
    /// The source identity this entry dedupes on (QA05-06): equal sources
    /// are the same event arriving twice, however differently they render.
    pub source: WakeSource,
    /// The validated event artifact.
    pub artifact: EventArtifact,
}

impl QueuedWake {
    /// The wake reason this event would cause.
    #[must_use]
    pub fn wake_reason(&self) -> WakeReason {
        self.artifact.kind.wake_reason()
    }

    /// The source identity of the event.
    #[must_use]
    pub fn source(&self) -> &WakeSource {
        &self.source
    }
}

/// What [`WakeQueue::push`] did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Accepted as a new pending entry.
    Queued {
        /// Queue depth after the push.
        depth: usize,
    },
    /// An identical artifact was already pending — deduplicated, not queued
    /// twice (a watcher double-fire is not two events).
    MergedDuplicate {
        /// The seq of the pending entry it matched.
        seq: u64,
    },
    /// The bounded queue was full: refused (never silently dropped, never
    /// reordered). The caller records/traces it.
    Overflow {
        /// The bound that was hit.
        cap: usize,
    },
}

/// One evaluation's worth of merged work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeBatch {
    /// The queue seqs folded into this batch, ascending.
    pub seqs: Vec<u64>,
    /// The single artifact the packet will carry.
    pub artifact: EventArtifact,
}

impl WakeBatch {
    /// The wake reason (a merged batch carries its members' shared reason).
    #[must_use]
    pub fn wake_reason(&self) -> WakeReason {
        self.artifact.kind.wake_reason()
    }

    /// How many events this one evaluation covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seqs.len()
    }

    /// Always false: a batch is taken from a non-empty queue.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seqs.is_empty()
    }
}

/// Counters for the wake plane (telemetry, never orchestration state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    /// The configured bound.
    pub cap: usize,
    /// Events waiting for the next evaluation.
    pub depth: usize,
    /// Deduped duplicate arrivals.
    pub merged_duplicates: u64,
    /// Refused arrivals (queue full).
    pub overflowed: u64,
    /// Events folded into a shared evaluation (merged batches).
    pub merged_into_batch: u64,
}

/// The bounded, merging Mayor wake queue (ADR-0028 cl. 2–3).
///
/// FIFO, deterministic, capped: `push` deduplicates by **source identity**
/// and refuses at `cap`; `take_batch` folds a maximal prefix of same-reason
/// events into one artifact while the merged body fits
/// [`MAX_ARTIFACT_BYTES`], splitting (never truncating) at the cap.
#[derive(Debug)]
pub struct WakeQueue {
    cap: usize,
    next_seq: u64,
    pending: VecDeque<QueuedWake>,
    merged_duplicates: u64,
    overflowed: u64,
    merged_into_batch: u64,
}

impl WakeQueue {
    /// A queue bounded to `cap` pending events.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            next_seq: 1,
            pending: VecDeque::new(),
            merged_duplicates: 0,
            overflowed: 0,
            merged_into_batch: 0,
        }
    }

    /// The configured bound.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Events waiting for the next evaluation.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    /// Is anything waiting?
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Counters for telemetry.
    #[must_use]
    pub fn stats(&self) -> QueueStats {
        QueueStats {
            cap: self.cap,
            depth: self.pending.len(),
            merged_duplicates: self.merged_duplicates,
            overflowed: self.overflowed,
            merged_into_batch: self.merged_into_batch,
        }
    }

    /// Offer one event.
    ///
    /// Dedupe is on **source identity** (QA05-06), not on rendered content:
    /// a watcher double-fire is the same event arriving twice, while two
    /// different events that happen to render identically — the same sandbox
    /// quarantined twice for the same owner, two identical crash lines — are
    /// two events and both reach the Mayor. Identity is per-plane (spool
    /// path, webhook correlation, decree id + notice number, crash
    /// `uid/session/restarts`); see [`WakeSource`].
    pub fn push(&mut self, event: WakeEvent) -> PushOutcome {
        if self.pending.iter().any(|q| q.source == event.source) {
            let seq = self
                .pending
                .iter()
                .find(|q| q.source == event.source)
                .map_or(0, |q| q.seq);
            self.merged_duplicates += 1;
            return PushOutcome::MergedDuplicate { seq };
        }
        if self.pending.len() >= self.cap {
            self.overflowed += 1;
            return PushOutcome::Overflow { cap: self.cap };
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push_back(QueuedWake {
            seq,
            source: event.source,
            artifact: event.artifact,
        });
        PushOutcome::Queued {
            depth: self.pending.len(),
        }
    }

    /// Take the next evaluation's worth of work, or `None` when idle.
    ///
    /// Folds the maximal FIFO prefix whose events share a wake reason *and*
    /// whose merged artifact still fits the cap. Mixed reasons and oversize
    /// members end the batch — the remainder waits for the next wake, in
    /// order.
    pub fn take_batch(&mut self) -> Option<WakeBatch> {
        let first = self.pending.front()?.clone();
        let reason = first.wake_reason();
        let mut members: Vec<BatchMember> = Vec::new();
        let mut seqs: Vec<u64> = Vec::new();
        // Fold a maximal FIFO prefix: same wake reason, and a merged body
        // that still fits the artifact cap. The first head is always
        // admitted (its own artifact is already capped by construction), so
        // the loop always makes progress and terminates.
        while let Some(head) = self.pending.front().cloned() {
            if head.wake_reason() != reason {
                break; // a mixed reason keeps its own wake
            }
            let candidate = EventArtifact::merged(
                reason,
                std::iter::once(BatchMember {
                    kind: head.artifact.kind.clone(),
                    body: head.artifact.body.clone(),
                })
                .chain(members.iter().map(|m| BatchMember {
                    kind: m.kind.clone(),
                    body: m.body.clone(),
                }))
                .collect(),
            );
            if candidate.is_err() {
                break; // would overflow the cap: the remainder waits
            }
            members.push(BatchMember {
                kind: head.artifact.kind.clone(),
                body: head.artifact.body.clone(),
            });
            seqs.push(head.seq);
            self.pending.pop_front();
        }
        debug_assert!(!members.is_empty() && members.len() == seqs.len());
        // A single-event batch renders as that event, not as a one-member
        // batch: the unmerged packet form stays the common case and the merge
        // header appears only when it means something.
        let artifact = if seqs.len() == 1 {
            first.artifact.clone()
        } else {
            EventArtifact::merged(reason, members).unwrap_or_else(|_| first.artifact.clone())
        };
        if seqs.len() > 1 {
            self.merged_into_batch += 1;
        }
        Some(WakeBatch { seqs, artifact })
    }
}

// ---------------------------------------------------------------------------
// Packet builder
// ---------------------------------------------------------------------------

/// A packet ready to be the Mayor's single turn: the typed packet, the
/// exact rendered prompt bytes, and the wake it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPacket {
    /// Derived from the artifact.
    pub wake_reason: WakeReason,
    /// The typed packet (MAYOR.md + state grid + one artifact).
    pub packet: ContextPacket,
    /// The rendered prompt — byte-exact what the session receives.
    pub text: String,
}

impl RenderedPacket {
    /// Rendered prompt size in bytes.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.text.len()
    }
}

/// Builds the Mayor's context packet from the tree (SPEC §7; ADR-0027).
///
/// Reads `MAYOR.md` with ADR-0007 lstat discipline (regular file or typed
/// refusal), projects the charter into a [`StateGrid`], and renders one
/// packet per [`WakeBatch`]. Size guards are ADR-0027's caps (config-tunable
/// queue bound aside, the prompt caps are schema-fixed): every overrun is a
/// typed refusal, never a shortened prompt.
#[derive(Debug, Clone)]
pub struct PacketBuilder {
    tree: HamletTree,
}

impl PacketBuilder {
    /// Builder over a village tree.
    #[must_use]
    pub fn new(tree: HamletTree) -> Self {
        Self { tree }
    }

    /// The village tree.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Read the trunk prompt `keep/census/manor/MAYOR.md` verbatim.
    ///
    /// # Errors
    ///
    /// [`MayorError::MissingMayorMd`] when absent,
    /// [`MayorError::NotRegularFile`] for a symlink/dir/other,
    /// [`MayorError::Packet`] for an empty or oversize prompt.
    pub fn mayor_md(&self) -> Result<String, MayorError> {
        let path = self.tree.manor_mayor_md();
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => return Err(MayorError::NotRegularFile { path }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MayorError::MissingMayorMd { path })
            }
            Err(source) => {
                return Err(MayorError::Io {
                    context: format!("stat {}", path.display()),
                    source,
                })
            }
        }
        let text = fs::read_to_string(&path).map_err(io_err(format!("read {}", path.display())))?;
        // Reuse the packet's own guard so an empty/oversize MAYOR.md is
        // refused with the schema's vocabulary, not a bespoke one.
        EventArtifact::new(EventKind::CommissionCreated { commission: 0 }, &text)?;
        Ok(text)
    }

    /// Project the charter into the state grid (a pure read).
    ///
    /// # Errors
    ///
    /// [`MayorError::Charter`] on load/validate failure.
    pub fn state_grid(&self) -> Result<StateGrid, MayorError> {
        Ok(StateGrid::from_charter(&Charter::load(
            &self.tree.charter(),
        )?))
    }

    /// Build one packet for `batch`.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — charter, MAYOR.md, or packet-cap failures.
    pub fn build(&self, batch: &WakeBatch) -> Result<ContextPacket, MayorError> {
        Ok(ContextPacket::new(
            self.mayor_md()?,
            self.state_grid()?,
            batch.artifact.clone(),
        )?)
    }

    /// Build and render one packet for `batch`.
    ///
    /// # Errors
    ///
    /// As [`PacketBuilder::build`], plus a render-cap refusal.
    pub fn render(&self, batch: &WakeBatch) -> Result<RenderedPacket, MayorError> {
        let packet = self.build(batch)?;
        let text = packet.render()?;
        Ok(RenderedPacket {
            wake_reason: packet.wake_reason,
            packet,
            text,
        })
    }
}

// ---------------------------------------------------------------------------
// One single-turn evaluation
// ---------------------------------------------------------------------------

/// How a single Mayor turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    /// pi reported the task boundary (`agent_settled`) — the normal end.
    Settled,
    /// pi answered the prompt with `success: false` (refused, no run).
    Refused,
    /// stdout closed before the task boundary.
    Eof,
    /// `mayor.turn_timeout_ms` elapsed; the process was killed (ADR-0028
    /// cl. 1: never resumed with `--continue`).
    TimedOut,
}

/// The observable record of one evaluation: everything the tree shows
/// happened, and nothing it didn't.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    /// The one-shot session identity (UID 1000, one incarnation per wake).
    pub session: SessionId,
    /// The prompt's correlation id (`m-<seq>-<uid>.<inc>`; wire id `…:p`).
    pub correlation: CorrelationId,
    /// Wake reason this turn answered.
    pub wake_reason: WakeReason,
    /// Events folded into this turn (1 = unmerged).
    pub events: usize,
    /// Rendered prompt bytes delivered.
    pub prompt_bytes: usize,
    /// Stream frames folded (telemetry shape).
    pub frames: u32,
    /// Malformed frames tolerated (never fatal, ADR-0015).
    pub malformed: u32,
    /// The last cumulative usage snapshot pi reported, when any.
    pub usage: Option<Usage>,
    /// How the turn ended.
    pub end: TurnEnd,
    /// The process exit code (`SIGNAL_EXIT_CODE` when signalled).
    pub exit_code: i32,
}

impl TurnOutcome {
    /// Did the turn reach pi's task boundary?
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.end == TurnEnd::Settled
    }
}

/// What one [`MayorEngine::wake`] call did (SPEC §7 wake accounting).
#[derive(Debug, Clone, PartialEq)]
pub enum WakeOutcome {
    /// This caller held the turn lock and ran the evaluation.
    Evaluated(Box<TurnOutcome>),
    /// Another evaluation was in flight and its drain covered this event
    /// (the merge, not a dropped wake).
    Coalesced,
    /// The bounded queue was full: refused, traced, and **not** evaluated.
    Overflow {
        /// The bound that was hit (`mayor.wake_queue_depth`).
        cap: usize,
    },
}

/// What the daemon knows about the Mayor's sleep — the zero-token state.
///
/// Between wakes the Mayor holds **no process**: `Awake` is false and
/// nothing can stream. `Pending` is how many events wait for the next
/// evaluation (SPEC §7 sleeps at zero token cost *while events queue*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepState {
    /// True only while one evaluation holds the turn lock.
    pub awake: bool,
    /// Events waiting for the next evaluation.
    pub pending: usize,
    /// Evaluations completed by this process (incarnations == wakes).
    pub turns: u64,
}

// ---------------------------------------------------------------------------
// The Mayor engine
// ---------------------------------------------------------------------------

/// The Mayor wake engine: bounded queue + packet builder + one-shot
/// evaluation, serialized to one evaluation at a time (SPEC §7).
pub struct MayorEngine {
    tree: HamletTree,
    config: Config,
    packets: PacketBuilder,
    queue: Mutex<WakeQueue>,
    /// One evaluation at a time. Held for the whole spawn→settle→exit
    /// window, which is what makes the Mayor *serial*: two concurrent
    /// watches can never produce two turns.
    turn: AsyncMutex<()>,
    spawner: Arc<dyn SessionSpawner>,
    env: BTreeMap<String, String>,
    telemetry: Option<Arc<Telemetry>>,
    turns: AtomicU64,
    charter_lock: Mutex<()>,
}

impl MayorEngine {
    /// Engine over a village tree, the engine config, and the session
    /// spawn seam ([`PodmanSpawner`] in production,
    /// [`HostProcessSpawner`](crate::supervise::HostProcessSpawner) over
    /// the FakeAgent in tests).
    #[must_use]
    pub fn new(tree: HamletTree, config: Config, spawner: Arc<dyn SessionSpawner>) -> Self {
        let cap = config.mayor.wake_queue_depth;
        Self {
            packets: PacketBuilder::new(tree.clone()),
            tree,
            config,
            queue: Mutex::new(WakeQueue::new(cap)),
            turn: AsyncMutex::new(()),
            spawner,
            env: BTreeMap::new(),
            telemetry: None,
            turns: AtomicU64::new(0),
            charter_lock: Mutex::new(()),
        }
    }

    /// Attach the structural (non-secret) manor env the exec carries; the
    /// `.env` secrets are merged per wake by the caller's
    /// [`Secrets`](hamlet_core::config::Secrets) mapping, exactly like a
    /// parcel session (SPEC §11.4: secrets ride `-e` env only).
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Attach the sled telemetry sink for Mayor traces (optional; traces
    /// degrade to stderr when absent — telemetry loss never corrupts
    /// state, ADR-0005 cl. 6).
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// The village tree.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// The packet builder (read-only inspection).
    #[must_use]
    pub fn packets(&self) -> &PacketBuilder {
        &self.packets
    }

    /// The engine config in force.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The observable sleep state (zero-token between wakes).
    #[must_use]
    pub fn sleep_state(&self) -> SleepState {
        SleepState {
            awake: self.turn.try_lock().is_err(),
            pending: self.queue.lock().unwrap_or_else(|e| e.into_inner()).depth(),
            turns: self.turns.load(Ordering::Relaxed),
        }
    }

    /// Wake counters (telemetry).
    #[must_use]
    pub fn queue_stats(&self) -> QueueStats {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).stats()
    }

    /// Accept one event and (whoever gets the turn lock first) evaluate.
    ///
    /// Serialization: the turn lock is held across the whole evaluation, so
    /// events arriving during a turn queue and are merged into the next one
    /// — five mails inside one turn's window are one wake with five
    /// artifacts, not five turns.
    ///
    /// # Errors
    ///
    /// [`MayorError`] — charter/MAYOR.md/packet refusals, spawn or protocol
    /// failures. The event stays queued on a *packet* failure (it may be
    /// buildable after the operator fixes `MAYOR.md`), and is consumed only
    /// once a batch was actually taken.
    pub async fn wake(&self, event: WakeEvent) -> Result<WakeOutcome, MayorError> {
        let reason = event.wake_reason();
        {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            match queue.push(event) {
                PushOutcome::Queued { .. } | PushOutcome::MergedDuplicate { .. } => {}
                PushOutcome::Overflow { cap } => {
                    self.trace(
                        "mayor-wake-overflow",
                        format!("{reason} refused at cap {cap} — event dropped, traced"),
                    );
                    return Ok(WakeOutcome::Overflow { cap });
                }
            }
        }
        let _guard = self.turn.lock().await;
        let batch = {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.take_batch()
        };
        let Some(batch) = batch else {
            // Someone else drained the queue while we waited — the events
            // were evaluated by the holder (merged), not lost.
            self.trace(
                "mayor-wake-coalesced",
                format!("{reason} coalesced into the in-flight evaluation"),
            );
            return Ok(WakeOutcome::Coalesced);
        };
        self.trace(
            "mayor-wake",
            format!(
                "reason={reason} events={} bytes_pending={}",
                batch.len(),
                self.sleep_state().pending
            ),
        );
        let rendered = self.packets.render(&batch)?;
        self.evaluate_locked(&rendered, batch.len())
            .await
            .map(|turn| WakeOutcome::Evaluated(Box::new(turn)))
    }

    /// One wake = one fresh invocation = one turn (ADR-0028 cl. 1).
    ///
    /// Sequence: charter `Starting` (new incarnation) → spawn `pi --mode
    /// rpc` over the exec seam → readiness probe → write the packet as
    /// exactly one prompt → fold the stream to the task boundary → stdin
    /// EOF → reap → charter `Stopped`. The process never outlives the call,
    /// so nothing of it survives into the next wake.
    ///
    /// # Errors
    ///
    /// [`MayorError`] on spawn/readiness/turn/packet failures; the charter
    /// is left consistent (`Crashed` on failure paths).
    pub async fn evaluate(
        &self,
        rendered: &RenderedPacket,
        events: usize,
    ) -> Result<TurnOutcome, MayorError> {
        let _guard = self.turn.lock().await;
        self.evaluate_locked(rendered, events).await
    }

    /// The turn itself, with the serialization lock already held by the
    /// caller ([`MayorEngine::wake`] / [`MayorEngine::evaluate`]).
    async fn evaluate_locked(
        &self,
        rendered: &RenderedPacket,
        events: usize,
    ) -> Result<TurnOutcome, MayorError> {
        let session = self.begin_session()?;
        let spec = SessionExecSpec {
            container: self.manor_container(),
            uid: MAYOR_UID,
            env: self.env.clone(),
            // No `--continue`, ever: pi history must never enter a turn
            // (ADR-0028 cl. 1). `pi_session_cmd` never adds it.
            cmd: pi_session_cmd(&self.config.sessions),
        };
        let outcome = self.run_turn(&spec, session, rendered, events).await;
        self.turns.fetch_add(1, Ordering::Relaxed);
        outcome
    }

    /// Restart-resumability (CONVENTIONS §3 r6): after a daemon restart,
    /// a Mayor session recorded *alive* in the charter is an orphan (its
    /// exec died with the old daemon) — mark it `Crashed` and trace.
    /// Returns the healed uids. Refuses while an evaluation is live.
    ///
    /// # Errors
    ///
    /// [`MayorError::Charter`] on load/validate/save failure,
    /// [`MayorError::Session`] on an illegal transition.
    pub fn recover(&self) -> Result<Vec<u32>, MayorError> {
        if self.sleep_state().awake {
            return Err(MayorError::Session(SessionError::NotAlive(
                SessionStatus::Running,
            )));
        }
        let mut healed = Vec::new();
        self.update_charter(|charter| {
            let Some(record) = charter.sessions.get(&MAYOR_UID).copied() else {
                return Ok(());
            };
            if !record.status.is_alive() {
                return Ok(());
            }
            let status = record.status.transition(SessionStatus::Crashed)?;
            charter
                .sessions
                .insert(MAYOR_UID, SessionRecord { status, ..record });
            healed.push(MAYOR_UID);
            Ok(())
        })?;
        if !healed.is_empty() {
            self.trace(
                "mayor-session-orphaned-recovered",
                "Mayor session marked crashed after daemon restart".to_owned(),
            );
        }
        Ok(healed)
    }

    fn manor_container(&self) -> String {
        self.tree
            .manor()
            .file_name()
            .map_or_else(|| "manor".to_owned(), |n| n.to_string_lossy().into_owned())
    }

    fn update_charter<T>(
        &self,
        f: impl FnOnce(&mut Charter) -> Result<T, MayorError>,
    ) -> Result<T, MayorError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.tree.charter();
        let mut charter = Charter::load(&path)?;
        let out = f(&mut charter)?;
        charter.validate()?;
        charter.save_atomic(&path)?;
        Ok(out)
    }

    fn trace(&self, event: &str, detail: impl Into<String>) {
        let Some(telemetry) = &self.telemetry else {
            eprintln!("hamlet-daemon: mayor {event}: {}", detail.into());
            return;
        };
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: event.to_owned(),
            detail: detail.into(),
        };
        if let Err(e) = telemetry.trace_event(
            AgentKey {
                commission: 0,
                uid: MAYOR_UID,
            },
            record,
        ) {
            eprintln!("hamlet-daemon: mayor trace {event} failed: {e}");
        }
    }

    /// Charter: bump the incarnation and persist `Starting` (CONVENTIONS §3
    /// r2 — the charter is truth about the Mayor's own life, one incarnation
    /// per wake).
    fn begin_session(&self) -> Result<SessionId, MayorError> {
        let identity = mayor_identity()?;
        self.update_charter(|charter| {
            if !charter.slots.contains_key(&MAYOR_UID) {
                // The manor must be provisioned first; a wake without a slot
                // is a boot-order bug, not something to paper over.
                return Err(MayorError::NoMayorSlot);
            }
            let next = match charter.sessions.get(&MAYOR_UID).copied() {
                None => SessionId::first(MAYOR_UID)?,
                Some(record) => {
                    let status = if record.status.is_alive() {
                        record.status.transition(SessionStatus::Crashed)?
                    } else {
                        record.status
                    };
                    let healed = SessionRecord { status, ..record };
                    let next = healed.id.next_incarnation()?;
                    charter.sessions.insert(MAYOR_UID, healed);
                    next
                }
            };
            if let Some(prev) = charter.sessions.get(&MAYOR_UID) {
                prev.status.transition(SessionStatus::Starting)?;
            }
            charter.sessions.insert(
                MAYOR_UID,
                SessionRecord {
                    id: next,
                    status: SessionStatus::Starting,
                    restart_count: 0,
                },
            );
            charter
                .agents
                .entry(identity.clone())
                .or_insert(AgentRef { slot: MAYOR_UID });
            Ok(next)
        })
    }

    fn save_status(&self, session: SessionId, status: SessionStatus) {
        if let Err(e) = self.update_charter(|charter| {
            let Some(record) = charter.sessions.get(&MAYOR_UID).copied() else {
                return Ok(());
            };
            if record.id != session {
                // A newer incarnation owns the slot now; never regress it.
                return Ok(());
            }
            let status = record.status.transition(status)?;
            charter
                .sessions
                .insert(MAYOR_UID, SessionRecord { status, ..record });
            Ok(())
        }) {
            self.trace("mayor-charter-write-failed", e.to_string());
        }
    }

    /// The whole single-turn protocol over one freshly spawned life.
    ///
    /// The loop has exactly three phases — ready, one prompt, settle — and
    /// leaves through exactly one of `Settled` / `Refused` / `Eof` /
    /// `TimedOut`. There is no second prompt by construction: the packet is
    /// the turn (SPEC §7, single-turn).
    async fn run_turn(
        &self,
        spec: &SessionExecSpec,
        session: SessionId,
        rendered: &RenderedPacket,
        events: usize,
    ) -> Result<TurnOutcome, MayorError> {
        let mut handle = self.spawner.spawn_session(spec).await?;
        let Some(pipes) = handle.take_pipes() else {
            self.save_status(session, SessionStatus::Crashed);
            return Err(MayorError::NoPipes);
        };
        // stderr is diagnostics only, never protocol (ADR-0015 cl. 1).
        let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        tokio::spawn(drain_stderr(pipes.stderr, Arc::clone(&stderr_tail)));

        let mut reader = BufReader::new(pipes.stdout).lines();
        let mut stdin = Some(pipes.stdin);
        let grace = TokioDuration::from_millis(self.config.sessions.stop_grace_ms);
        let bound = TokioDuration::from_millis(self.config.mayor.turn_timeout_ms);

        let correlation = CorrelationId::for_queued(QueuedKind::Tasking, &session, 1)?;
        let envelope = SessionEnvelope::tasking(&session, 1, &rendered.text)?;
        let prompt_id = correlation.wire_id("p");
        let probe_id = session.probe_id();

        let mut frames = 0u32;
        let mut malformed = 0u32;
        let mut usage: Option<Usage> = None;
        let mut ready = false;
        let mut prompted = false;

        let mut finish: Option<TurnEnd> = None;
        let protocol = async {
            // --- readiness probe (pi emits no banner, ADR-0015 cl. 3) ----
            let probe = readiness_probe(&session);
            write_record(
                stdin.as_mut().expect("stdin is owned"),
                &probe.to_json_line(),
            )
            .await?;

            while finish.is_none() {
                let Ok(Some(line)) = reader.next_line().await else {
                    finish = Some(TurnEnd::Eof);
                    break;
                };
                let event = parse_agent_event(&line);
                frames += 1;
                if let AgentEvent::MessageUpdate {
                    usage: Some(snapshot),
                } = &event
                {
                    usage = Some(*snapshot);
                }
                if let AgentEvent::Malformed { .. } = &event {
                    malformed += 1;
                }
                match &event {
                    AgentEvent::Response {
                        id,
                        command,
                        success,
                        error,
                        ..
                    } => {
                        if !ready && id.as_deref() == Some(probe_id.as_str()) {
                            ready = true;
                            if !*success {
                                return Err(MayorError::ReadinessRefused);
                            }
                            self.save_status(session, SessionStatus::Running);
                            // Exactly one prompt per turn.
                            for record in pi_binding(&envelope, SessionStatus::Running)? {
                                write_record(
                                    stdin.as_mut().expect("stdin is owned"),
                                    &record.to_json_line(),
                                )
                                .await?;
                            }
                            prompted = true;
                        } else if prompted && id.as_deref() == Some(prompt_id.as_str()) && !*success
                        {
                            self.trace(
                                "mayor-prompt-refused",
                                format!("command {command} refused: {error:?}"),
                            );
                            finish = Some(TurnEnd::Refused);
                        }
                    }
                    AgentEvent::AgentStart if prompted => {
                        self.save_status(session, SessionStatus::Busy);
                    }
                    AgentEvent::AgentSettled if prompted => {
                        self.save_status(session, SessionStatus::Idle);
                        finish = Some(TurnEnd::Settled);
                    }
                    _ => {}
                }
            }
            Ok(())
        };

        // The turn bound (`mayor.turn_timeout_ms`) is the daemon-side wall
        // clock of an *observation*, never a state input (CONVENTIONS §3 r4):
        // it decides whether this process is reaped, not what the village
        // believes.
        let protocol: Result<(), MayorError> = match timeout(bound, protocol).await {
            Ok(result) => result,
            Err(_) => {
                finish = Some(TurnEnd::TimedOut);
                Ok(())
            }
        };

        if let Err(e) = protocol {
            // Abandon the turn: EOF + reap, so no process survives the error.
            drop(stdin.take());
            let code = reap(&mut handle).await;
            self.finish_crashed(session, e.to_string());
            self.trace(
                "mayor-turn-exited",
                format!("session={session} exit={code} after failure: {e}"),
            );
            return Err(e);
        }

        // --- sleep: EOF on stdin is pi's orderly end, then reap ----------
        drop(stdin.take());
        let end = finish.unwrap_or(TurnEnd::Eof);
        let exit_code = if end == TurnEnd::TimedOut {
            let _ = handle.kill();
            reap(&mut handle).await
        } else {
            match timeout(grace, handle.wait()).await {
                Ok(Ok(code)) => code,
                Ok(Err(e)) => {
                    self.finish_crashed(session, e.to_string());
                    return Err(MayorError::ExitFailed(e));
                }
                Err(_) => {
                    self.finish_crashed(session, "exit grace window elapsed".to_owned());
                    return Err(MayorError::ExitTimeout);
                }
            }
        };
        if matches!(end, TurnEnd::Settled | TurnEnd::Refused) {
            self.save_status(session, SessionStatus::Stopped);
        } else {
            self.finish_crashed(session, format!("turn ended {end:?}"));
        }
        let outcome = TurnOutcome {
            session,
            correlation,
            wake_reason: rendered.wake_reason,
            events,
            prompt_bytes: rendered.bytes(),
            frames,
            malformed,
            usage,
            end,
            exit_code,
        };
        self.trace(
            "mayor-turn",
            format!(
                "session={} reason={} events={} bytes={} frames={} malformed={malformed} end={end:?} exit={exit_code}",
                session,
                outcome.wake_reason,
                outcome.events,
                outcome.prompt_bytes,
                outcome.frames
            ),
        );
        Ok(outcome)
    }

    fn finish_crashed(&self, session: SessionId, detail: String) {
        self.save_status(session, SessionStatus::Crashed);
        self.trace(
            "mayor-turn-crashed",
            format!("session={session} · {}", scrub_detail(&detail)),
        );
    }
}

/// Wait for the process to exit (best-effort hard kill), resolving to its
/// exit code (`SIGNAL_EXIT_CODE` when signalled).
async fn reap(handle: &mut Box<dyn crate::podman::ExecHandle>) -> i32 {
    let _ = handle.kill();
    match handle.wait().await {
        Ok(code) => code,
        Err(_) => SIGNAL_EXIT_CODE,
    }
}

async fn write_record(
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    line: &str,
) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// Bounded stderr tail (diagnostics only — never protocol, never secrets).
async fn drain_stderr(
    mut stderr: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    tail: Arc<Mutex<Vec<u8>>>,
) {
    let mut chunk = [0u8; 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                tail.extend(chunk.iter().take(n));
                const CAP: usize = 2048;
                while tail.len() > CAP {
                    tail.remove(0);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wake-hook adapters (fire-and-forget, ADR-0021 cl. 5 / ADR-0022 cl. 6)
// ---------------------------------------------------------------------------

/// Bridges routed mail for the Mayor's slot to the engine (SPEC §7, §8.1).
///
/// The router calls this with the **still-unacked spool path**: the artifact
/// is loaded from the file on disk, so the packet's `spool:` label and its
/// body are the same bytes the courier wrote.
#[derive(Clone)]
pub struct MayorMailBridge {
    engine: Arc<MayorEngine>,
}

impl MayorMailBridge {
    /// Bridge an engine into the mail plane.
    #[must_use]
    pub fn new(engine: Arc<MayorEngine>) -> Self {
        Self { engine }
    }
}

impl crate::mailroute::MayorMail for MayorMailBridge {
    fn mayor_mail(&self, spool: &std::path::Path) {
        let engine = Arc::clone(&self.engine);
        let tree = engine.tree().clone();
        let spool = spool.to_path_buf();
        tokio::spawn(async move {
            match WakeEvent::from_mail_spool(&tree, &spool) {
                Ok(event) => {
                    if let Err(e) = engine.wake(event).await {
                        eprintln!("hamlet-daemon: mayor mail wake failed: {e}");
                    }
                }
                Err(e) => eprintln!("hamlet-daemon: mayor mail wake refused: {e}"),
            }
        });
    }
}

/// Bridges the freight audit's [`MayorWake`] hook to the engine. The hook is
/// synchronous and must never block the audit on Mayor liveness, so the
/// event is handed to the engine on its own task.
#[derive(Clone)]
pub struct FreightWakeBridge {
    engine: Arc<MayorEngine>,
}

impl FreightWakeBridge {
    /// Bridge an engine into the freight audit.
    #[must_use]
    pub fn new(engine: Arc<MayorEngine>) -> Self {
        Self { engine }
    }
}

impl MayorWake for FreightWakeBridge {
    fn wake(&self, escalation: &FreightEscalation) {
        let engine = Arc::clone(&self.engine);
        let escalation = escalation.clone();
        tokio::spawn(async move {
            match WakeEvent::from_freight_escalation(&escalation) {
                Ok(event) => {
                    if let Err(e) = engine.wake(event).await {
                        eprintln!("hamlet-daemon: mayor freight wake failed: {e}");
                    }
                }
                Err(e) => eprintln!("hamlet-daemon: mayor freight wake refused: {e}"),
            }
        });
    }
}

/// Bridges the inbound-webhook hook ([`WebhookWake`], ADR-0022 cl. 6) to the
/// engine: the artifact is loaded from the tree's own inbox envelope, never
/// from an in-memory copy of the request.
#[derive(Clone)]
pub struct WebhookWakeBridge {
    engine: Arc<MayorEngine>,
}

impl WebhookWakeBridge {
    /// Bridge an engine into the webhook plane.
    #[must_use]
    pub fn new(engine: Arc<MayorEngine>) -> Self {
        Self { engine }
    }
}

impl WebhookWake for WebhookWakeBridge {
    fn webhook(&self, event: &WebhookEvent) {
        let engine = Arc::clone(&self.engine);
        let tree = engine.tree().clone();
        let event = event.clone();
        tokio::spawn(async move {
            let topic = match sanitize_name(&event.topic) {
                Ok(topic) => topic,
                Err(e) => {
                    eprintln!("hamlet-daemon: mayor webhook wake refused topic: {e}");
                    return;
                }
            };
            // The webhook plane already rejects unconfigured topics with 404
            // (ADR-0022 cl. 3); this is the second, config-driven guard so a
            // route can never reach the Mayor without being named for it.
            if !engine.config().webhooks.routes.contains_key(topic.as_str()) {
                eprintln!(
                    "hamlet-daemon: mayor webhook wake ignored topic {topic}: not in \
                     webhooks.routes"
                );
                return;
            }
            match WakeEvent::from_webhook_inbox(&tree, topic, &event.correlation) {
                Ok(wake) => {
                    if let Err(e) = engine.wake(wake).await {
                        eprintln!("hamlet-daemon: mayor webhook wake failed: {e}");
                    }
                }
                Err(e) => eprintln!("hamlet-daemon: mayor webhook wake refused: {e}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hamlet_core::mail::MailSender;

    #[test]
    fn template_and_identity_are_committed_constants() {
        assert!(
            MAYOR_MD_TEMPLATE.len() > 1024,
            "the v1 trunk prompt is a real prompt"
        );
        assert!(MAYOR_MD_TEMPLATE.starts_with("# MAYOR — Trunk Agent, UID 1000"));
        assert_eq!(mayor_identity().unwrap().as_str(), "manor");
    }

    /// Dedupe is on **source identity**, so a double fire of the same event
    /// is one queue entry while two events that merely render alike are two
    /// (QA05-06): here the spool path is the identity, so the same subject
    /// body delivered under two spool files is two wakes.
    #[test]
    fn queue_dedupes_by_source_identity_and_bounds_depth() {
        let mut queue = WakeQueue::new(2);
        let mail = |subject: &str, spool: &str| {
            WakeEvent::new(
                EventKind::MailReceived {
                    from: MailSender::Uid(1001),
                    subject: subject.to_owned(),
                    spool: TreeRelPath::parse(spool).unwrap(),
                },
                "body\n",
            )
            .unwrap()
        };
        let spool_a = "courier/mail/1000/000000000007-1001-ab12.mail";
        let spool_b = "courier/mail/1000/000000000008-1001-ab12.mail";
        let spool_c = "courier/mail/1000/000000000009-1001-ab12.mail";
        assert!(matches!(
            queue.push(mail("a", spool_a)),
            PushOutcome::Queued { depth: 1 }
        ));
        assert!(
            matches!(
                queue.push(mail("a", spool_a)),
                PushOutcome::MergedDuplicate { seq: 1 }
            ),
            "the same spool file re-announced is one event, not two"
        );
        assert!(
            matches!(
                queue.push(mail("b", spool_b)),
                PushOutcome::Queued { depth: 2 }
            ),
            "a byte-identical body under a different spool file is a different event"
        );
        assert_eq!(
            queue.push(mail("c", spool_c)),
            PushOutcome::Overflow { cap: 2 },
            "a bounded queue refuses rather than growing"
        );
        let stats = queue.stats();
        assert_eq!(stats.overflowed, 1);
        assert_eq!(stats.merged_duplicates, 1);
        assert_eq!(queue.depth(), 2);
    }

    #[test]
    fn batch_is_fifo_same_reason_and_size_bounded() {
        let mail = |n: u64, body: String| {
            WakeEvent::new(
                EventKind::MailReceived {
                    from: MailSender::Uid(1001),
                    subject: format!("m{n}"),
                    spool: TreeRelPath::parse("courier/mail/1000/000000000007-1001-ab12.mail")
                        .unwrap(),
                },
                body,
            )
            .unwrap()
        };
        // Two small mails merge; a third that would overflow the artifact cap
        // splits into the next wake instead of truncating the prompt.
        let mut queue = WakeQueue::new(16);
        queue.push(mail(1, "a".repeat(20_000)));
        queue.push(mail(2, "b".repeat(20_000)));
        queue.push(mail(3, "c".repeat(40_000)));
        let batch = queue.take_batch().unwrap();
        assert_eq!(batch.len(), 2, "the third mail would overflow the cap");
        assert_eq!(batch.artifact.body.matches("--- artifact ").count(), 2);
        let rest = queue.take_batch().unwrap();
        assert_eq!(rest.len(), 1);
        assert!(queue.take_batch().is_none());
    }

    #[test]
    fn batch_never_mixes_wake_reasons() {
        let mut queue = WakeQueue::new(8);
        queue.push(
            WakeEvent::new(
                EventKind::MailReceived {
                    from: MailSender::Uid(1001),
                    subject: "m".to_owned(),
                    spool: TreeRelPath::parse("courier/mail/1000/000000000007-1001-ab12.mail")
                        .unwrap(),
                },
                "body\n",
            )
            .unwrap(),
        );
        queue.push(
            WakeEvent::new(EventKind::CommissionCreated { commission: 2 }, "brief\n").unwrap(),
        );
        let first = queue.take_batch().unwrap();
        assert_eq!(first.wake_reason(), WakeReason::MailToMayor);
        let second = queue.take_batch().unwrap();
        assert_eq!(second.wake_reason(), WakeReason::CommissionCreated);
    }
}
