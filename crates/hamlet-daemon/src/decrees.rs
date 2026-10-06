//! Decree interpretation and execution (SPEC §7; ADR-0026 cl. 4–6,
//! ADR-0029).
//!
//! The Mayor never runs anything. It writes **files** into
//! `keep/census/manor/home/decrees/`, and this module is the daemon half of
//! that contract: watch → validate → replay-check → execute → acknowledge.
//! Every step is observable from the tree alone (CONVENTIONS §2
//! file-as-truth): a decree file, the charter's `decrees`/`decree_receipts`
//! rows, the `<decree_id>.ack` the Mayor reads on its next wake, and — for
//! refusals that have no usable identity — a hidden `.rejected/` quarantine
//! entry.
//!
//! # The pipeline
//!
//! ```text
//! Reaction::DecreeWrite ─▶ scan manor_decrees() (sorted = charter order)
//!   per file, in `seq` order:
//!     1. read (size-capped) ─▶ parse ─▶ DECREE schema ok?  ─── no ─▶ .rejected/
//!     2. target pre-flight (register/blueprint/freight/slot/evidence) ─ no ─▶ ack rejected
//!     3. charter.admit_decree(id)          (replay algebra, ADR-0026 cl. 3)
//!          Replayed + delivered → re-ack `done`, execute NOTHING
//!          Replayed + queued    → resume: execute again (executors are idempotent)
//!          Replayed + failed    → re-ack `rejected: execution_failed`
//!     4. execute (no charter writes inside)  ─── error ─▶ ack rejected + Failed
//!     5. charter.set_decree_status(Delivered) then write `<id>.ack` (atomic)
//! ```
//!
//! Ordering is the charter counter, never mtime/clock (CONVENTIONS §3 r4),
//! and `dec-<seq>` file names sort lexicographically in that order.
//!
//! # Validate fully, then execute
//!
//! Pre-flight (step 2) resolves *every* structural dependency of a decree —
//! register brief, blueprint declaration, freight file, charter slot, live
//! session, evidence refs — **before** any side effect. A decree that fails
//! pre-flight is acknowledged `rejected` and nothing has happened, so the
//! Mayor never sees half a provisioning.
//!
//! # Idempotence and crash resume
//!
//! The receipt is written with the `queued` status in **one atomic charter
//! save** (CONVENTIONS §3 r2), so "admitted" and "queued" cannot disagree.
//! If the daemon dies mid-decree the status is still `queued` on restart,
//! the scan re-admits it as `Replayed`, and the executor runs the (idempotent)
//! executor again before acknowledging. Every executor here is convergent:
//! provision adopts, stop is idempotent, enqueue is retried, the escalation
//! file is rewritten byte-identically, the ack is rewritten byte-identically.
//!
//! # Seams
//!
//! Sprint 06 owns deliverable promotion and Sprint 07 owns hibernation
//! copies, so those two go through traits ([`Promotion`], [`Stasis`]) with
//! honest in-tree stubs — a stub acknowledges `deferred`, never a fake
//! `done` (ADR-0029 cl. 3/4).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, DecreeAdmission, DecreeStatus};
use hamlet_core::config::{Config, Secrets};
use hamlet_core::decree::{
    Decree, DecreeAck, DecreeError, DecreeId, EscalationNotice, RejectReason, TreeRelPath,
    ESCALATION_VERSION, MAX_DECREE_BYTES,
};
use hamlet_core::dispatch::{Method, OutboxRequest, DISPATCH_VERSION};
use hamlet_core::mail::is_slot_uid;
use hamlet_core::sanitize::SafeName;
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;

use crate::census::Provisioner;
use crate::delivery::QueueError;
use crate::podman::BoxFuture;
use crate::supervise::{scrub_detail, CrashNotice, CrashWake, SuperviseError, Supervisor};
use crate::telemetry::{AgentKey, Telemetry, TraceRecord};
use crate::watch::{Dispatcher, WatcherHandle};

/// Handler registration name (dispatcher fan-out is sorted by name).
pub const HANDLER_NAME: &str = "decrees";

/// `O_NOFOLLOW` for the open in [`DecreeExecutor::read_decree`].
///
/// Named as a constant rather than pulled from `libc`: SPEC §3 locks the
/// crate stack and adding a dependency needs its own ADR. The value is the
/// kernel's on every platform this daemon ships for (Linux, all arches and
/// ABIs), and an open with an unknown flag bit is refused — never ignored —
/// so a wrong value here fails loud, not open.
#[cfg(target_os = "linux")]
const O_NOFOLLOW_FLAG: i32 = 0o400_000;
#[cfg(not(target_os = "linux"))]
compile_error!("the decree plane requires a POSIX O_NOFOLLOW open (SPEC §7, ADR-0007)");

fn libc_nofollow() -> i32 {
    O_NOFOLLOW_FLAG
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A decree the daemon could not execute. `reason` is the stable
/// acknowledgment token (ADR-0026 cl. 5); `detail` is the operator-facing
/// trace line (scrubbed, never mail/artifact content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecreeRefusal {
    /// The stable rejection reason written into the ack file.
    pub reason: RejectReason,
    /// One-line, scrubbed explanation (telemetry only).
    pub detail: String,
}

impl DecreeRefusal {
    /// A refusal with its stable reason and a scrubbed detail line.
    #[must_use]
    pub fn new(reason: RejectReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: scrub_detail(&detail.into()),
        }
    }

    /// Structural refusal: the decree names something that does not exist or
    /// cannot exist (ADR-0026 cl. 5 `invalid_target`).
    #[must_use]
    pub fn invalid_target(detail: impl Into<String>) -> Self {
        Self::new(RejectReason::InvalidTarget, detail)
    }

    /// The target UID holds no charter slot (SPEC §4).
    #[must_use]
    pub fn unknown_slot(uid: u32) -> Self {
        Self::new(
            RejectReason::UnknownSlot,
            format!("no charter slot for uid {uid}"),
        )
    }

    /// The executor ran and failed (slot exhaustion, runtime failure).
    #[must_use]
    pub fn execution_failed(detail: impl Into<String>) -> Self {
        Self::new(RejectReason::ExecutionFailed, detail)
    }

    /// Understood but not yet implementable (Sprint 06 promotion).
    #[must_use]
    pub fn deferred(detail: impl Into<String>) -> Self {
        Self::new(RejectReason::Deferred, detail)
    }
}

impl fmt::Display for DecreeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.reason, self.detail)
    }
}

/// Failure of the executor itself (tree/charter/telemetry plumbing), as
/// opposed to a decree being refused.
#[derive(Debug)]
pub enum ExecutorError {
    /// Charter load/validate/save failure.
    Charter(hamlet_core::charter::CharterError),
    /// Filesystem failure with the step that failed.
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// A path that must be a regular file is a symlink/dir/other (ADR-0007).
    NotRegularFile {
        /// The offending path.
        path: PathBuf,
    },
    /// The decree directory could not be vouched for: it is a planted
    /// symlink, it resolves outside the manor home, or it is writable by
    /// someone other than its owner (QA05-01). The whole pass fails closed.
    Provenance {
        /// The directory that failed the check.
        path: PathBuf,
        /// One-line, scrubbed explanation.
        detail: String,
    },
    /// Canonical rendering of a daemon-written artifact failed.
    Render(DecreeError),
    /// Telemetry write failed (telemetry degrades, but a *scan* still reports
    /// it rather than hiding it).
    Telemetry(String),
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Charter(e) => write!(f, "decrees: charter: {e}"),
            Self::Io { context, source } => write!(f, "decrees: {context}: {source}"),
            Self::NotRegularFile { path } => write!(
                f,
                "decrees: {} must be a regular file; refusing to follow or adopt (ADR-0007)",
                path.display()
            ),
            Self::Render(e) => write!(f, "decrees: render: {e}"),
            Self::Provenance { path, detail } => write!(
                f,
                "decrees: {} is not a provable decree directory ({detail}); no decree is \
                 executed from it (QA05-01, ADR-0007)",
                path.display()
            ),
            Self::Telemetry(e) => write!(f, "decrees: telemetry: {e}"),
        }
    }
}

impl std::error::Error for ExecutorError {}

impl From<hamlet_core::charter::CharterError> for ExecutorError {
    fn from(e: hamlet_core::charter::CharterError) -> Self {
        Self::Charter(e)
    }
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> ExecutorError {
    move |source| ExecutorError::Io {
        context: context.into(),
        source,
    }
}

// ---------------------------------------------------------------------------
// Executor seams
// ---------------------------------------------------------------------------

/// What a `provision_agent` decree asks for, fully resolved from the tree
/// before any side effect (SPEC §5, §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionRequest {
    /// Register identity — the immutable brief stem in `guild/register/`.
    pub identity: SafeName,
    /// Commission the agent serves (a `null` commission is refused: Sprint
    /// 02 provisioning is commission-shaped).
    pub commission: u64,
    /// The parsed blueprint declaration from `guild/blueprints/<ref>.toml`.
    pub blueprint: Blueprint,
    /// The register brief verbatim. The production seam appends the seeded
    /// `PROPRIETOR.md` before the first session (it only knows the parcel's
    /// UID after provisioning); Sprint 06 appends INDENTURE entries to the
    /// same brief.
    pub brief: String,
}

/// What a `provision_agent` decree produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvisionOutcome {
    /// The permanent slot the identity now holds (SPEC §4).
    pub uid: u32,
    /// The session incarnation the first life started as.
    pub incarnation: u32,
}

/// The provisioning seam: Sprint 02 parcel provisioning + Sprint 03 session
/// start, behind one idempotent call.
///
/// Idempotence: re-provisioning an identity adopts intact state (charter
/// slot per identity, stub documents adopted, never clobbered) and
/// `start_session` on a crashed/stopped parcel bumps the incarnation and
/// resets the restart budget. A second call is therefore safe.
pub trait AgentProvisioner: Send + Sync {
    /// Provision the parcel and start its first session.
    ///
    /// # Errors
    ///
    /// [`DecreeRefusal`] with the typed reason the Mayor should see.
    fn provision<'a>(
        &'a self,
        request: ProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionOutcome, DecreeRefusal>>;
}

/// The session-routing seam: the Sprint 03 delivery queue (`reassign_task`)
/// and the Sprint 03 interrupt path (`steer_session`, source = Mayor).
pub trait SessionRouter: Send + Sync {
    /// Enqueue a bounded task assignment for a slot (sequential stdin
    /// delivery, `QueuedKind::Tasking`).
    ///
    /// # Errors
    ///
    /// [`DecreeRefusal`] on queue overflow, an absent slot, or a write
    /// failure.
    fn reassign(
        &self,
        uid: u32,
        commission: u64,
        entry: &SafeName,
        text: &str,
    ) -> Result<(), DecreeRefusal>;

    /// Inject `interrupt_and_steer` down a live session (SPEC §6 Divine
    /// Interruption), attributed to the Mayor.
    ///
    /// # Errors
    ///
    /// [`DecreeRefusal`] when the session is not live / not ready, or the
    /// write fails.
    fn steer<'a>(
        &'a self,
        uid: u32,
        directive: &'a str,
    ) -> BoxFuture<'a, Result<(), DecreeRefusal>>;
}

/// The hibernation seam (SPEC §10; ADR-0029 cl. 3).
///
/// Sprint 07 owns the *copy*: parking a home under
/// `tavern/patrons/PROPRIETOR<name>/home.uid.bak/`, waking it back. What
/// exists today — and what this sprint implements behind the seam — is the
/// clean stop plus the charter status flip (`parcels[uid].stasis`), which
/// is what makes the slot "asleep" to every other subsystem.
///
/// **Sprint 07 hook (recorded):** implement [`Stasis::park`] as
/// *clean stop → `podman unshare` copy of `parcel<uid>/home/` → charter
/// `stasis: true`* and [`Stasis::wake`] as the inverse, then delete
/// [`CleanStopStasis`]. No caller changes: the executor only knows the trait.
pub trait Stasis: Send + Sync {
    /// Park one agent: stop its session cleanly and mark its lease as in
    /// stasis (charter `parcels[uid].stasis = true`).
    ///
    /// # Errors
    ///
    /// [`DecreeRefusal`] when the slot is absent, already parked, or the
    /// stop failed.
    fn park<'a>(&'a self, uid: u32) -> BoxFuture<'a, Result<(), DecreeRefusal>>;
}

/// The promotion seam (SPEC §7 validation loop; ADR-0029 cl. 3).
///
/// Sprint 06 owns this: the INDENTURE-bound validation module, the
/// vault copy, and the DEED lineage entry. Until it lands the executor's
/// in-tree stub records the decree and acknowledges `deferred` — the
/// charter status stays `queued`, so the decree is executed for real the
/// moment the seam is implemented. Acknowledging `done` for work that did
/// not happen would be the one unforgivable lie in this protocol.
pub trait Promotion: Send + Sync {
    /// Approve a deliverable for promotion into `keep/vault/`.
    ///
    /// # Errors
    ///
    /// [`DecreeRefusal`] — `deferred` from the Sprint 06 stub.
    fn approve(&self, freight_path: &str, artifact_name: &SafeName) -> Result<(), DecreeRefusal>;

    /// Reject a deliverable; the reason goes back to the owning agent.
    ///
    /// # Errors
    ///
    /// [`DecreeRefusal`] — `deferred` from the Sprint 06 stub.
    fn reject(&self, freight_path: &str, reason: &str) -> Result<(), DecreeRefusal>;
}

// ---------------------------------------------------------------------------
// Production seam implementations
// ---------------------------------------------------------------------------

/// Production [`SessionRouter`] over the Sprint 03 supervisor: the delivery
/// queue for tasking, the interrupt path for steering.
///
/// `reassign` uses `QueuedKind::Tasking` — the same sequential per-recipient
/// funnel as mail, so a reassigned task cannot interleave with a message
/// mid-record. `steer` uses [`InterruptSource::Mayor`](hamlet_core::session::InterruptSource::Mayor),
/// which is wire-visible on the interrupt envelope (ADR-0015 cl. 3), and
/// `Normal` priority: a Mayor steer does not silently drop an agent's queued
/// follow-ups.
pub struct SupervisorRouter {
    supervisor: Arc<Supervisor>,
}

impl SupervisorRouter {
    /// Router over the daemon's one supervisor.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }
}

impl SessionRouter for SupervisorRouter {
    fn reassign(
        &self,
        uid: u32,
        _commission: u64,
        _entry: &SafeName,
        text: &str,
    ) -> Result<(), DecreeRefusal> {
        self.supervisor
            .enqueue(uid, hamlet_core::session::QueuedKind::Tasking, text)
            .map(|_| ())
            .map_err(|e| match e {
                SuperviseError::Queue(QueueError::Overflow { depth, .. }) => DecreeRefusal::new(
                    RejectReason::ExecutionFailed,
                    format!("uid {uid} queue full at {depth} undelivered envelopes"),
                ),
                SuperviseError::NoSlot(uid) => DecreeRefusal::unknown_slot(uid),
                other => DecreeRefusal::execution_failed(format!("uid {uid}: {other}")),
            })
    }

    fn steer<'a>(
        &'a self,
        uid: u32,
        directive: &'a str,
    ) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        use hamlet_core::session::{InterruptPriority, InterruptSource};
        let supervisor = Arc::clone(&self.supervisor);
        Box::pin(async move {
            supervisor
                .steer(
                    uid,
                    InterruptSource::Mayor,
                    InterruptPriority::Normal,
                    directive,
                )
                .await
                .map(|_| ())
                .map_err(|e| match e {
                    SuperviseError::NoSlot(uid) => DecreeRefusal::unknown_slot(uid),
                    other => DecreeRefusal::execution_failed(format!("steer uid {uid}: {other}")),
                })
        })
    }
}

/// The production provisioning seam: [`Provisioner`] (Sprint 02, with full
/// rollback) then `Supervisor::start_session` (Sprint 03) with the brief the
/// Mayor's decree implies.
///
/// The session brief is **register brief + seeded `PROPRIETOR.md`** (SPEC §5):
/// the identity an agent is provisioned from, and the mutable record
/// provisioning seeded for it. INDENTURE.md scanning arrives in Sprint 06
/// and is appended to this same brief.
pub struct CensusAgentProvisioner {
    provisioner: Arc<Provisioner>,
    supervisor: Arc<Supervisor>,
    secrets: Secrets,
}

impl CensusAgentProvisioner {
    /// Provisioner over the census, supervisor, and `.env` secrets.
    #[must_use]
    pub fn new(
        provisioner: Arc<Provisioner>,
        supervisor: Arc<Supervisor>,
        secrets: Secrets,
    ) -> Self {
        Self {
            provisioner,
            supervisor,
            secrets,
        }
    }
}

impl AgentProvisioner for CensusAgentProvisioner {
    fn provision<'a>(
        &'a self,
        request: ProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionOutcome, DecreeRefusal>> {
        Box::pin(self.provision_inner(request))
    }
}

impl CensusAgentProvisioner {
    async fn provision_inner(
        &self,
        request: ProvisionRequest,
    ) -> Result<ProvisionOutcome, DecreeRefusal> {
        let lease = self
            .provisioner
            .provision_parcel(&request.identity, request.commission, &request.blueprint)
            .await
            .map_err(|e| {
                use crate::census::ProvisionError;
                match e {
                    ProvisionError::Slot(_) => DecreeRefusal::execution_failed(e.to_string()),
                    _ => DecreeRefusal::execution_failed(format!("provision failed: {e}")),
                }
            })?;
        // `provision_parcel` already recorded the lease keyed by the slot it
        // allocated; the identity is permanent per slot (SPEC §4), so the
        // charter is the only place that mapping exists (file-as-truth).
        let uid = self.charter_uid(&request.identity).map_err(|e| {
            DecreeRefusal::execution_failed(format!("slot for {}: {e}", request.identity))
        })?;
        let _ = lease;
        // The session brief is **register + seeded PROPRIETOR** (SPEC §5):
        // the identity definition plus the mutable record `provision_parcel`
        // just seeded at `parcel<uid>/PROPRIETOR.md`. INDENTURE scanning
        // appends to this same brief in Sprint 06.
        let register =
            std::fs::read_to_string(self.provisioner.tree().register_brief(&request.identity))
                .map_err(|e| {
                    DecreeRefusal::execution_failed(format!("register brief unreadable: {e}"))
                })?;
        let proprietor = std::fs::read_to_string(self.provisioner.tree().parcel_proprietor(uid))
            .map_err(|e| {
                DecreeRefusal::execution_failed(format!("seeded PROPRIETOR unreadable: {e}"))
            })?;
        let brief = format!("{register}\n--- PROPRIETOR SEEDS (parcel{uid}) ---\n{proprietor}");
        let parcel = self
            .supervisor
            .parcel_ref(uid)
            .map_err(|e| DecreeRefusal::execution_failed(format!("parcel ref: {e}")))?;
        let session = self
            .supervisor
            .start_session(&parcel, &brief, &self.secrets)
            .await
            .map_err(|e| DecreeRefusal::execution_failed(format!("session start: {e}")))?;
        Ok(ProvisionOutcome {
            uid: parcel.uid,
            incarnation: session.incarnation,
        })
    }

    /// The permanent slot an identity holds, read from the charter.
    fn charter_uid(&self, identity: &SafeName) -> Result<u32, hamlet_core::charter::CharterError> {
        let charter = Charter::load(&self.provisioner.tree().charter())?;
        charter
            .slots
            .iter()
            .find(|(_, slot)| &slot.identity == identity)
            .map(|(uid, _)| *uid)
            .ok_or_else(|| hamlet_core::charter::CharterError::Invariant {
                message: format!("no charter slot for identity {identity} after provisioning"),
            })
    }
}

/// The hibernate executor as it exists in Sprint 05: a **clean stop** of
/// the agent's session plus the charter lease flip to `stasis: true`
/// (SPEC §10). The home copy is Sprint 07's job — see [`Stasis`].
///
/// Idempotent: stopping an already-stopped session is a no-op, and flipping
/// an already-parked lease is a no-op, so a replayed `hibernate_agent`
/// decree converges instead of erroring.
pub struct CleanStopStasis {
    supervisor: Arc<Supervisor>,
}

impl CleanStopStasis {
    /// Seam over the daemon's one supervisor.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }
}

impl Stasis for CleanStopStasis {
    fn park<'a>(&'a self, uid: u32) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        Box::pin(self.park_inner(uid))
    }
}

impl CleanStopStasis {
    async fn park_inner(&self, uid: u32) -> Result<(), DecreeRefusal> {
        let record = self
            .supervisor
            .session_record(uid)
            .map_err(|e| DecreeRefusal::execution_failed(format!("session record: {e}")))?;
        if let Some(record) = record {
            if record.status.is_alive() {
                // A clean stop: drain + EOF + reap inside the grace window
                // (SPEC §6). `stop_session` is the typed path; it is
                // idempotent for an already-dead slot.
                self.supervisor
                    .stop_session(uid)
                    .await
                    .map_err(|e| DecreeRefusal::execution_failed(format!("stop_session: {e}")))?;
            }
        }
        Ok(())
    }
}

/// Recording promotion stub (Sprint 06 replaces it; ADR-0029 cl. 3).
///
/// It records **what** was asked for — the decree id is already in the
/// charter — and refuses with `deferred`. No vault copy, no DEED entry, no
/// fake acknowledgment.
#[derive(Default)]
pub struct DeferredPromotion {
    approvals: Mutex<Vec<(String, SafeName)>>,
    rejections: Mutex<Vec<(String, String)>>,
}

impl DeferredPromotion {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `(freight_path, artifact_name)` of every approval asked for.
    pub fn approvals(&self) -> Vec<(String, SafeName)> {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `(freight_path, reason)` of every rejection asked for.
    pub fn rejections(&self) -> Vec<(String, String)> {
        self.rejections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Promotion for DeferredPromotion {
    fn approve(&self, freight_path: &str, artifact_name: &SafeName) -> Result<(), DecreeRefusal> {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((freight_path.to_owned(), artifact_name.clone()));
        Err(DecreeRefusal::deferred(format!(
            "approve_promotion of {freight_path} → {artifact_name}: SPEC §7 validation module \
             lands in Sprint 06 (deferred, status stays queued)"
        )))
    }

    fn reject(&self, freight_path: &str, reason: &str) -> Result<(), DecreeRefusal> {
        self.rejections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((freight_path.to_owned(), reason.to_owned()));
        Err(DecreeRefusal::deferred(format!(
            "reject_promotion of {freight_path}: SPEC §7 validation module lands in Sprint 06 \
             (deferred, status stays queued)"
        )))
    }
}

// ---------------------------------------------------------------------------
// Execution report
// ---------------------------------------------------------------------------

/// What happened to one decree file in a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecreeOutcome {
    /// Executed now (or resumed after a crash) and acknowledged `done`.
    Executed {
        /// The decree identity.
        id: DecreeId,
        /// Which vocabulary variant ran.
        kind: &'static str,
    },
    /// Already executed with identical content: re-acknowledged `done`
    /// without touching the world (ADR-0026 cl. 4).
    Replayed {
        /// The decree identity.
        id: DecreeId,
    },
    /// Refused with a typed reason; the ack file carries it.
    Rejected {
        /// The decree identity.
        id: DecreeId,
        /// The stable rejection reason.
        reason: RejectReason,
    },
    /// The file had no usable identity: renamed into `.rejected/` and
    /// traced. Never acknowledged — there is no id to acknowledge.
    Quarantined {
        /// The file name as found.
        file: String,
        /// Why it could not be parsed.
        reason: RejectReason,
    },
    /// The file failed to parse, but it has **changed** since the last pass
    /// refused it: a write still in flight (QA05-05). Nothing was done to it;
    /// the next pass looks again and quarantines it if it has stopped
    /// changing.
    ParseDeferred {
        /// The file name as found.
        file: String,
        /// Why it could not be parsed.
        reason: RejectReason,
    },
    /// The file cannot be attributed to the Mayor slot (QA05-01). It is left
    /// **exactly where it is** — not executed, not acknowledged, not
    /// quarantined, no sequence consumed — and traced, because a directory
    /// the daemon cannot vouch for is exactly where a wrong "cleanup" write
    /// would do the most damage.
    Unattributable {
        /// The file name as found.
        file: String,
        /// Always [`RejectReason::ForeignProvenance`].
        reason: RejectReason,
    },
}

impl DecreeOutcome {
    /// The decree identity, when the outcome has one.
    #[must_use]
    pub fn id(&self) -> Option<&DecreeId> {
        match self {
            Self::Executed { id, .. } | Self::Replayed { id } | Self::Rejected { id, .. } => {
                Some(id)
            }
            Self::Quarantined { .. } | Self::ParseDeferred { .. } | Self::Unattributable { .. } => {
                None
            }
        }
    }
}

/// One full pass over the decree directory (CONVENTIONS §3 r1: every count
/// and every outcome is derived, sorted, never accumulated).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionReport {
    /// Outcomes in file (i.e. charter-sequence) order.
    pub outcomes: Vec<DecreeOutcome>,
}

impl ExecutionReport {
    /// Outcomes of one kind.
    pub fn count(&self, predicate: impl Fn(&DecreeOutcome) -> bool) -> usize {
        self.outcomes.iter().filter(|o| predicate(o)).count()
    }

    /// Decrees executed (or resumed) in this pass.
    #[must_use]
    pub fn executed(&self) -> usize {
        self.count(|o| matches!(o, DecreeOutcome::Executed { .. }))
    }

    /// Idempotent no-ops (identical replay).
    #[must_use]
    pub fn replayed(&self) -> usize {
        self.count(|o| matches!(o, DecreeOutcome::Replayed { .. }))
    }

    /// Refused decrees (typed ack written).
    #[must_use]
    pub fn rejected(&self) -> usize {
        self.count(|o| matches!(o, DecreeOutcome::Rejected { .. }))
    }

    /// Files quarantined for want of an identity.
    #[must_use]
    pub fn quarantined(&self) -> usize {
        self.count(|o| matches!(o, DecreeOutcome::Quarantined { .. }))
    }

    /// Files whose parse failure was treated as a torn write: left in place,
    /// to be re-read next pass.
    #[must_use]
    pub fn parse_deferred(&self) -> usize {
        self.count(|o| matches!(o, DecreeOutcome::ParseDeferred { .. }))
    }

    /// Files refused as unattributable to the Mayor slot (QA05-01).
    #[must_use]
    pub fn unattributable(&self) -> usize {
        self.count(|o| matches!(o, DecreeOutcome::Unattributable { .. }))
    }
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// The singular decree interpreter (CONVENTIONS §2: only the daemon mutates
/// a live tree). Built once per village and shared as `Arc`.
///
/// Construction is explicit about which seams exist: a village with no
/// promotion subsystem still runs the executor, and every promotion decree
/// is acknowledged `deferred` instead of silently dropped.
pub struct DecreeExecutor {
    tree: HamletTree,
    config: Config,
    provisioner: Mutex<Arc<dyn AgentProvisioner>>,
    router: Mutex<Arc<dyn SessionRouter>>,
    stasis: Mutex<Arc<dyn Stasis>>,
    promotion: Mutex<Arc<dyn Promotion>>,
    telemetry: Option<Arc<Telemetry>>,
    /// The runtime seam, used **only** to write into subuid-owned trees
    /// (`manor/home/`) from inside the daemon's own userns (ADR-0011 cl. 4,
    /// QA05-02). Absent in unit tests that never touch a 0700 manor home.
    runtime: Mutex<Option<Arc<dyn crate::podman::Runtime>>>,
    charter_lock: Mutex<()>,
    /// Serializes whole passes (`scan`); charter writes have their own
    /// short lock.
    exec_lock: tokio::sync::Mutex<()>,
    /// First-strike fingerprints of unparseable decree files, keyed by
    /// file name (QA05-05): a file whose (len, mtime) is *unchanged* since
    /// the last failed parse is a finished bad file; a file that is still
    /// changing is a torn write and must not be quarantined out from under
    /// its writer.
    torn: Mutex<BTreeMap<String, TornFingerprint>>,
}

/// A file's observed shape at the moment it failed to parse. Two observations
/// that match mean "this file has not changed since we last refused it" —
/// the only evidence that distinguishes a finished bad file from a write in
/// flight, without ever reading a clock for anything but this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TornFingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl DecreeExecutor {
    /// An executor over `tree` with every seam still empty — callers install
    /// them with the `with_*` builders. Used by tests that only exercise
    /// refusals, and as the safe default.
    #[must_use]
    pub fn new(tree: HamletTree, config: Config) -> Self {
        Self {
            tree,
            config,
            provisioner: Mutex::new(Arc::new(DeferredProvisioner)),
            router: Mutex::new(Arc::new(RefusingRouter)),
            stasis: Mutex::new(Arc::new(RefusingStasis)),
            promotion: Mutex::new(Arc::new(DeferredPromotion::new())),
            telemetry: None,
            runtime: Mutex::new(None),
            charter_lock: Mutex::new(()),
            exec_lock: tokio::sync::Mutex::new(()),
            torn: Mutex::new(BTreeMap::new()),
        }
    }

    /// Install the provisioning seam.
    #[must_use]
    pub fn with_provisioner(mut self, provisioner: Arc<dyn AgentProvisioner>) -> Self {
        self.provisioner = Mutex::new(provisioner);
        self
    }

    /// Install the session-routing seam (queue + interrupt).
    #[must_use]
    pub fn with_router(mut self, router: Arc<dyn SessionRouter>) -> Self {
        self.router = Mutex::new(router);
        self
    }

    /// Install the hibernation seam.
    #[must_use]
    pub fn with_stasis(mut self, stasis: Arc<dyn Stasis>) -> Self {
        self.stasis = Mutex::new(stasis);
        self
    }

    /// Install the promotion seam.
    #[must_use]
    pub fn with_promotion(mut self, promotion: Arc<dyn Promotion>) -> Self {
        self.promotion = Mutex::new(promotion);
        self
    }

    /// Attach the sled telemetry sink (optional; traces degrade to stderr
    /// when absent — telemetry loss never corrupts state, ADR-0005 cl. 6).
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Install the runtime seam used for writes into the subuid-owned manor
    /// home (ADR-0011 cl. 4). Without it a village deployed under a
    /// dedicated service user cannot write an ack at all — so the executor
    /// reports the refusal loudly rather than pretending the Mayor was told.
    #[must_use]
    pub fn with_runtime(mut self, runtime: Arc<dyn crate::podman::Runtime>) -> Self {
        self.runtime = Mutex::new(Some(runtime));
        self
    }

    /// The village tree.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// The engine config in force (ack timeout, escalation webhook).
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    // --- the pass --------------------------------------------------------

    /// Scan the decree directory in charter-sequence order and bring every
    /// file to a terminal, acknowledged state.
    ///
    /// This is both the watcher reaction handler's body and the restart
    /// recovery: file-as-truth means a pass over the directory is always a
    /// complete statement of outstanding work (CONVENTIONS §2).
    ///
    /// # Errors
    ///
    /// [`ExecutorError`] only for executor plumbing (charter write, tree I/O).
    /// A decree that *cannot* be executed is an [`DecreeOutcome::Rejected`],
    /// not an error: one bad decree must never stop the queue.
    pub async fn scan(&self) -> Result<ExecutionReport, ExecutorError> {
        // One pass at a time, tree-wide: the recovery scan, the watcher
        // handler and an operator-triggered pass must never overlap (the
        // admit → execute window is not a single atomic charter save).
        let _guard = self.exec_lock.lock().await;
        let dir = self.tree.manor_decrees();
        // Provenance first, for the whole directory (QA05-01): if the decree
        // directory is not a plain, non-writable-by-others directory *inside
        // the manor home*, nothing in it is a decree the Mayor wrote, and
        // the pass refuses before it reads a single file.
        let dir_uid = self.verify_decree_dir()?;
        let mut names: Vec<String> = Vec::new();
        match fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|source| ExecutorError::Io {
                        context: format!("read {}", dir.display()),
                        source,
                    })?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Only well-formed decree files are inputs: acks, hidden
                    // scratch files and quarantined files are not decrees
                    // (ADR-0019 cl. 5 as amended by ADR-0026 cl. 6).
                    if hamlet_core::decree::is_decree_file_name(&name) {
                        names.push(name);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ExecutorError::Io {
                    context: format!("read {}", dir.display()),
                    source,
                })
            }
        }
        // Sorted names == sorted sequence (zero-padded seq, ADR-0026 cl. 3).
        names.sort();
        let mut report = ExecutionReport::default();
        let mut first_error: Option<ExecutorError> = None;
        for name in &names {
            let path = dir.join(name);
            // One decree's plumbing failure must not strand the decrees
            // behind it in the queue: the pass continues and reports the
            // first failure at the end. The failed file stays on disk, so
            // the next pass retries it from the tree (file-as-truth).
            match self.process_with(&path, dir_uid).await {
                Ok(outcome) => report.outcomes.push(outcome),
                Err(e) => {
                    self.trace("decree-pass-error", format!("{name}: {e}"));
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
        // Forget first-strike bookkeeping for files that no longer exist
        // (executed, quarantined, operator-cleaned) so the map cannot grow
        // without bound (QA05-05).
        {
            let mut torn = self.lock_torn();
            torn.retain(|name, _| names.contains(name));
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(report),
        }
    }

    /// Bring one decree file to a terminal state. Never panics on a bad
    /// file: an unparseable file is quarantined, a valid one is executed or
    /// refused with a typed ack.
    ///
    /// # Errors
    ///
    /// [`ExecutorError`] for charter/tree plumbing failures only.
    pub async fn process(&self, path: &Path) -> Result<DecreeOutcome, ExecutorError> {
        let dir_uid = self.verify_decree_dir()?;
        self.process_with(path, dir_uid).await
    }

    /// [`DecreeExecutor::process`] with the caller-verified decree-directory
    /// owner (the provenance fact every decree file is checked against).
    async fn process_with(
        &self,
        path: &Path,
        dir_uid: u32,
    ) -> Result<DecreeOutcome, ExecutorError> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parsed = self.read_decree(path, dir_uid);
        let decree = match parsed {
            Ok(decree) => decree,
            Err((reason, detail)) => {
                // An unattributable file is left exactly where it is: the
                // daemon is not entitled to move a file it cannot prove the
                // Mayor wrote, and quarantine is a write into a directory it
                // may not own (QA05-01).
                if reason == RejectReason::ForeignProvenance {
                    self.trace(
                        "decree-refused-unattributable",
                        format!("{name}: {reason} ({detail})"),
                    );
                    return Ok(DecreeOutcome::Unattributable { file: name, reason });
                }
                // Torn-write discipline (QA05-05): one refused parse is not
                // evidence of a *finished* bad file. Quarantine only when a
                // second pass sees byte-for-byte the same (len, mtime) it
                // refused last time; a file that is still changing is a write
                // in flight and is left alone.
                if !self.torn_is_stable(&name, path) {
                    self.trace(
                        "decree-parse-deferred",
                        format!("{name}: {reason} ({detail}); still writing, retry next pass"),
                    );
                    return Ok(DecreeOutcome::ParseDeferred { file: name, reason });
                }
                self.trace("decree-quarantined", format!("{name}: {reason} ({detail})"));
                self.quarantine(path, &name).await?;
                self.forget_torn(&name);
                return Ok(DecreeOutcome::Quarantined { file: name, reason });
            }
        };
        self.forget_torn(&name);
        let id = decree.id().clone();

        // --- pre-flight: resolve every structural dependency first --------
        if let Err(refusal) = self.preflight(&decree) {
            // No side effect has happened yet: acknowledging `rejected` is a
            // truthful statement about the world.
            self.reject(&id, &refusal).await?;
            self.trace("decree-rejected", format!("{id}: {refusal}"));
            return Ok(DecreeOutcome::Rejected {
                id,
                reason: refusal.reason,
            });
        }

        // --- replay algebra (ADR-0026 cl. 3) ------------------------------
        // Admission writes the receipt and the `queued` status in ONE
        // atomic charter save, so a crash can never leave "admitted but not
        // queued" (CONVENTIONS §3 r2).
        let prior_status = match self.admit(&id) {
            Ok(DecreeAdmission::Admitted) => None,
            Ok(DecreeAdmission::Replayed) => self
                .load_charter()?
                .decrees
                .get(&id.seq())
                .copied()
                .or(Some(DecreeStatus::Queued)),
            Err(refusal) => {
                // Out-of-order or a reused sequence: acknowledged, not
                // executed, and the sequence stays available.
                self.reject(&id, &refusal).await?;
                self.trace("decree-rejected", format!("{id}: {refusal}"));
                return Ok(DecreeOutcome::Rejected {
                    id,
                    reason: refusal.reason,
                });
            }
        };

        match prior_status {
            // Already executed, byte-identical content: acknowledge again,
            // touch nothing. This is the idempotence guarantee (ADR-0026
            // cl. 4) and the *steady state* of every rescan.
            Some(DecreeStatus::Delivered) => {
                self.write_ack(&DecreeAck::done(&id)).await?;
                self.trace(
                    "decree-replayed",
                    format!("{id}: already delivered, re-acked"),
                );
                Ok(DecreeOutcome::Replayed { id })
            }
            // Terminal failure from an earlier pass: acknowledge the same
            // verdict again, deterministically.
            Some(DecreeStatus::Failed) => {
                self.write_ack(&DecreeAck::rejected(&id, RejectReason::ExecutionFailed))
                    .await?;
                self.trace(
                    "decree-replayed",
                    format!("{id}: previously failed, re-acked as execution_failed"),
                );
                Ok(DecreeOutcome::Replayed { id })
            }
            // `None` (freshly admitted) or `Queued` (a crash left it open):
            // run the executor. Executors are idempotent, so a resume is
            // indistinguishable from a first run.
            None | Some(DecreeStatus::Queued) => {
                let resumed = prior_status.is_some();
                self.execute(&decree).await?;
                self.trace(
                    "decree-executed",
                    format!(
                        "{id}: {} ({})",
                        decree.kind(),
                        if resumed {
                            "resumed"
                        } else {
                            "first execution"
                        }
                    ),
                );
                Ok(DecreeOutcome::Executed {
                    id,
                    kind: decree.kind().token(),
                })
            }
        }
    }

    /// Read + parse + validate one decree file, **with provenance** (QA05-01).
    ///
    /// Three structural checks, then one attribution check: the file must be
    /// a regular file opened `O_NOFOLLOW` (ADR-0007), within the size cap,
    /// parseable — and owned by the same uid that owns the decree
    /// directory. `dir_uid` is what [`DecreeExecutor::verify_decree_dir`]
    /// established for the directory as a whole; comparing against it rather
    /// than against a hardcoded `MAYOR_UID` is what makes the check work
    /// under rootless podman, where subuid 1000 appears on the host as some
    /// other number (ADR-0011 cl. 2).
    ///
    /// Returns the refusal reason and a detail line. A file whose *name* is a
    /// well-formed decree id but whose body is broken still gets quarantined
    /// by the caller's structural path — the ack is written only when there
    /// is a parsed identity, and a file whose body cannot be trusted has none
    /// worth answering (ADR-0026 cl. 5).
    fn read_decree(&self, path: &Path, dir_uid: u32) -> Result<Decree, (RejectReason, String)> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let meta = fs::symlink_metadata(path)
            .map_err(|source| (RejectReason::Malformed, format!("stat failed: {source}")))?;
        if !meta.file_type().is_file() {
            return Err((
                RejectReason::Malformed,
                "not a regular file (symlink/dir/other refused, ADR-0007)".to_owned(),
            ));
        }
        if meta.uid() != dir_uid {
            return Err((
                RejectReason::ForeignProvenance,
                format!(
                    "decree file is owned by uid {}, the decree directory by uid {}: \
                     refusing a decree the daemon cannot attribute to the Mayor slot",
                    meta.uid(),
                    dir_uid
                ),
            ));
        }
        if meta.len() > MAX_DECREE_BYTES as u64 {
            return Err((
                RejectReason::Oversize,
                format!("{} bytes exceeds {MAX_DECREE_BYTES}", meta.len()),
            ));
        }
        // `O_NOFOLLOW`: the lstat above and this open are not one act, so
        // the file is re-opened with the kernel refusing to traverse a
        // planted symlink. Everything is re-checked on the *open* handle.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc_nofollow())
            .open(path)
            .map_err(|source| {
                (
                    RejectReason::Malformed,
                    format!(
                        "open (O_NOFOLLOW) failed: {}",
                        scrub_detail(&source.to_string())
                    ),
                )
            })?;
        let opened = file
            .metadata()
            .map_err(|source| (RejectReason::Malformed, format!("fstat failed: {source}")))?;
        if !opened.file_type().is_file() || opened.uid() != dir_uid {
            return Err((
                RejectReason::ForeignProvenance,
                format!(
                    "opened handle is not a regular uid-{dir_uid} file; refusing the \\
                     race between lstat and open"
                ),
            ));
        }
        if opened.len() > MAX_DECREE_BYTES as u64 {
            return Err((
                RejectReason::Oversize,
                format!("{} bytes exceeds {MAX_DECREE_BYTES}", opened.len()),
            ));
        }
        let mut raw = Vec::with_capacity(opened.len() as usize);
        use std::io::Read;
        file.take(MAX_DECREE_BYTES as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|source| {
                (
                    RejectReason::Malformed,
                    format!("read failed: {}", scrub_detail(&source.to_string())),
                )
            })?;
        let text = String::from_utf8_lossy(&raw);
        Decree::parse(&text).map_err(|e| (reject_reason_of(&e), e.to_string()))
    }

    /// Verify the decree **directory** before any file in it is read
    /// (QA05-01, ADR-0007): it must be a plain directory (never a symlink
    /// planted in place of it), it must really live under the manor home
    /// after canonicalization, and it must not be group- or world-writable.
    /// Returns the uid that owns it — the attribution every decree file is
    /// then checked against.
    ///
    /// A refusal here fails the **whole pass** closed: a decree directory
    /// the daemon cannot vouch for yields no executions, no acks and no
    /// quarantine, and the trace says why.
    fn verify_decree_dir(&self) -> Result<u32, ExecutorError> {
        use std::os::unix::fs::MetadataExt;
        let dir = self.tree.manor_decrees();
        let home = self.tree.manor_home();
        let deny = |detail: String| ExecutorError::Provenance {
            path: dir.clone(),
            detail: scrub_detail(&detail),
        };
        for (path, label) in [
            (home.as_path(), "manor home"),
            (dir.as_path(), "decree dir"),
        ] {
            match fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(deny(format!(
                        "{label} {} is not a plain directory (symlink/planted, ADR-0007)",
                        path.display()
                    )))
                }
                Err(source) => return Err(deny(format!("{label} {}: {source}", path.display()))),
            }
        }
        let canonical_dir = fs::canonicalize(&dir)
            .map_err(|source| deny(format!("canonicalize {}: {source}", dir.display())))?;
        let canonical_home = fs::canonicalize(&home)
            .map_err(|source| deny(format!("canonicalize {}: {source}", home.display())))?;
        if !canonical_dir.starts_with(&canonical_home) {
            return Err(deny(format!(
                "{} resolves to {canonical_dir:?}, outside the manor home {canonical_home:?}",
                dir.display()
            )));
        }
        let meta = fs::symlink_metadata(&dir)
            .map_err(|source| deny(format!("stat {}: {source}", dir.display())))?;
        if meta.mode() & 0o022 != 0 {
            return Err(deny(format!(
                "{} is mode {:04o}: group/world-writable decree directories are refused \
                 (another writer could author a decree)",
                dir.display(),
                meta.mode() & 0o7777
            )));
        }
        Ok(meta.uid())
    }

    /// Torn-write bookkeeping (QA05-05): record this failure's fingerprint
    /// and report whether an *earlier* pass refused this exact same shape.
    fn torn_is_stable(&self, name: &str, path: &Path) -> bool {
        let fingerprint = fs::metadata(path)
            .ok()
            .map(|meta| TornFingerprint {
                len: meta.len(),
                modified: meta.modified().ok(),
            })
            .unwrap_or(TornFingerprint {
                len: u64::MAX,
                modified: None,
            });
        let mut torn = self.lock_torn();
        match torn.get(name) {
            Some(previous) if *previous == fingerprint => true,
            _ => {
                torn.insert(name.to_owned(), fingerprint);
                false
            }
        }
    }

    fn forget_torn(&self, name: &str) {
        self.lock_torn().remove(name);
    }

    fn lock_torn(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, TornFingerprint>> {
        self.torn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Structural pre-flight: **no side effects**, full validation of every
    /// dependency the executor will need. A refusal here means nothing
    /// happened at all (the "never partial execution" rule).
    fn preflight(&self, decree: &Decree) -> Result<(), DecreeRefusal> {
        let charter = self
            .load_charter()
            .map_err(|e| DecreeRefusal::execution_failed(format!("charter unreadable: {e}")))?;
        match decree {
            Decree::ProvisionAgent {
                blueprint_ref,
                commission,
                register_ref,
                ..
            } => {
                let Some(commission) = commission else {
                    return Err(DecreeRefusal::invalid_target(
                        "provision_agent without a commission: Sprint 02 provisioning is \
                         commission-shaped (git bind, INDENTURE), so an idle agent has no \
                         executable path yet",
                    ));
                };
                if *commission == 0 {
                    return Err(DecreeRefusal::invalid_target(
                        "commission index must be >= 1",
                    ));
                }
                self.require_regular_file(&self.tree.register_brief(register_ref))?;
                self.require_regular_file(&self.tree.guild_blueprint(blueprint_ref))?;
                Ok(())
            }
            Decree::HibernateAgent { uid, .. } => {
                self.require_agent_slot(&charter, *uid)?;
                if !charter.parcels.contains_key(uid) {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "uid {uid} holds a slot but no parcel lease (SPEC §10)"
                    )));
                }
                Ok(())
            }
            Decree::ReassignTask {
                commission, uid, ..
            } => {
                self.require_agent_slot(&charter, *uid)?;
                if !charter.commissions.contains_key(commission) {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "commission {commission} is not in the charter ledger"
                    )));
                }
                Ok(())
            }
            Decree::SteerSession { uid, .. } => {
                self.require_agent_slot(&charter, *uid)?;
                let live = charter
                    .sessions
                    .get(uid)
                    .is_some_and(|record| record.status.is_alive());
                if !live {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "uid {uid} has no live session to steer (SPEC §6 interruption path)"
                    )));
                }
                Ok(())
            }
            Decree::EscalateToHuman { evidence_refs, .. } => {
                // The daemon re-resolves each evidence label against the
                // tree: an escalation that cites nothing is not evidence.
                for reference in evidence_refs {
                    let Some(resolved) = reference.join_under(self.tree.root()) else {
                        return Err(DecreeRefusal::invalid_target(format!(
                            "evidence ref {reference} escapes the village root"
                        )));
                    };
                    if !resolved.is_file() {
                        return Err(DecreeRefusal::invalid_target(format!(
                            "evidence ref {reference} does not exist in the tree"
                        )));
                    }
                }
                Ok(())
            }
            Decree::ApprovePromotion { freight_path, .. }
            | Decree::RejectPromotion { freight_path, .. } => {
                let Some(resolved) = freight_path.join_under(self.tree.root()) else {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "freight path {freight_path} escapes the village root"
                    )));
                };
                self.require_regular_file(&resolved)?;
                Ok(())
            }
        }
    }

    /// A charter slot for a *subagent* (never UID 1000: the Mayor is the
    /// trunk, it is not provisioned, hibernated, tasked or steered).
    fn require_agent_slot(&self, charter: &Charter, uid: u32) -> Result<(), DecreeRefusal> {
        if uid == MAYOR_UID {
            return Err(DecreeRefusal::invalid_target(
                "decree targets UID 1000: the Mayor is the trunk agent and is never a decree \
                 target (SPEC §4, §7)",
            ));
        }
        if !is_slot_uid(uid) {
            return Err(DecreeRefusal::invalid_target(format!(
                "uid {uid} is not a charter slot (1001..=2000)"
            )));
        }
        if !charter.slots.contains_key(&uid) {
            return Err(DecreeRefusal::unknown_slot(uid));
        }
        Ok(())
    }

    fn require_regular_file(&self, path: &Path) -> Result<(), DecreeRefusal> {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_file() => Ok(()),
            Ok(_) => Err(DecreeRefusal::invalid_target(format!(
                "{} is not a regular file (ADR-0007)",
                path.display()
            ))),
            Err(_) => Err(DecreeRefusal::invalid_target(format!(
                "{} does not exist",
                path.display()
            ))),
        }
    }

    /// Run one executor. **No charter writes happen inside**: the caller
    /// records the terminal status, so a mid-executor crash leaves the
    /// status `queued` and the next pass resumes.
    async fn execute(&self, decree: &Decree) -> Result<(), ExecutorError> {
        let id = decree.id().clone();
        match self.execute_inner(decree).await {
            Ok(()) => {
                if let Err(e) = self.finish(&id, DecreeStatus::Delivered) {
                    self.trace("decree-charter-write-failed", format!("{id}: {e}"));
                }
                // An ack that cannot be written is a HARD error (QA05-02):
                // the Mayor's loop stalls without it, and the ack-timeout
                // watcher would keep re-waking for a decree the daemon
                // already acted on. `write_ack` first tries the direct write
                // and falls back to the `podman unshare` seam when the
                // kernel refuses (a subuid-owned 0700 manor home, ADR-0011
                // cl. 4), so this only surfaces when *both* are impossible.
                // Replays retry it (Replayed + Delivered re-acks).
                self.write_ack(&DecreeAck::done(&id)).await?;
            }
            Err(refusal) => {
                // `deferred` keeps the decree open (status stays `queued`): the
                // seam lands in Sprint 06 and the pass then executes it.
                let status = if refusal.reason == RejectReason::Deferred {
                    DecreeStatus::Queued
                } else {
                    DecreeStatus::Failed
                };
                if let Err(e) = self.finish(&id, status) {
                    self.trace("decree-charter-write-failed", format!("{id}: {e}"));
                }
                self.write_ack(&DecreeAck::rejected(&id, refusal.reason))
                    .await?;
                self.trace("decree-refused", format!("{id}: {refusal}"));
            }
        }
        Ok(())
    }

    async fn execute_inner(&self, decree: &Decree) -> Result<(), DecreeRefusal> {
        match decree {
            Decree::ProvisionAgent {
                blueprint_ref,
                commission,
                register_ref,
                ..
            } => {
                let blueprint_text = fs::read_to_string(self.tree.guild_blueprint(blueprint_ref))
                    .map_err(|e| {
                    DecreeRefusal::invalid_target(format!(
                        "blueprint {} unreadable: {e}",
                        self.tree.guild_blueprint(blueprint_ref).display()
                    ))
                })?;
                let blueprint = Blueprint::from_toml(&blueprint_text).map_err(|e| {
                    DecreeRefusal::invalid_target(format!("blueprint {blueprint_ref} invalid: {e}"))
                })?;
                let Some(commission) = commission else {
                    return Err(DecreeRefusal::invalid_target(
                        "provision_agent without a commission",
                    ));
                };
                let brief_path = self.tree.register_brief(register_ref);
                let register_brief = fs::read_to_string(&brief_path).map_err(|e| {
                    DecreeRefusal::invalid_target(format!("register brief unreadable: {e}"))
                })?;
                // Sprint 02 allocates the slot, so the uid is only known after
                // provisioning — but the session brief must be built first.
                // The register brief *is* the identity definition; the
                // PROPRIETOR seeds are appended after the lease exists, in
                // `CensusAgentProvisioner`'s next revision (Sprint 06
                // appends INDENTURE entries to the same brief).
                let request = ProvisionRequest {
                    identity: register_ref.clone(),
                    commission: *commission,
                    blueprint,
                    brief: format!("--- REGISTER BRIEF ({register_ref}) ---\n{register_brief}"),
                };
                let outcome = {
                    let provisioner = self
                        .provisioner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    provisioner.provision(request).await?
                };
                self.trace(
                    "decree-provisioned",
                    format!(
                        "{} → uid {} incarnation {}",
                        decree.id(),
                        outcome.uid,
                        outcome.incarnation
                    ),
                );
                Ok(())
            }
            Decree::HibernateAgent { uid, .. } => {
                let stasis = self
                    .stasis
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                stasis.park(*uid).await.map(|()| self.flip_stasis(*uid))
            }
            Decree::ReassignTask {
                commission,
                entry,
                uid,
                ..
            } => {
                let text = format!(
                    "decree {} assigns you commission {commission} task {entry}.\n\
                     Read commission{commission}/INDENTURE.md entry {entry} for the scope, then \
                     report progress by mail to the Mayor (uid 1000).\n",
                    decree.id()
                );
                self.router
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .reassign(*uid, *commission, entry, &text)
            }
            Decree::SteerSession { directive, uid, .. } => {
                let router = self
                    .router
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                router.steer(*uid, directive).await
            }
            Decree::EscalateToHuman {
                evidence_refs,
                summary,
                ..
            } => {
                self.file_escalation(decree.id(), evidence_refs, summary)
                    .await
            }
            Decree::ApprovePromotion {
                artifact_name,
                freight_path,
                ..
            } => self
                .promotion
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .approve(&freight_path.to_string(), artifact_name),
            Decree::RejectPromotion {
                freight_path,
                reason,
                ..
            } => self
                .promotion
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reject(&freight_path.to_string(), reason),
        }
    }

    /// Flip `parcels[uid].stasis` — the charter half of hibernation
    /// (SPEC §10). The copy half is Sprint 07 behind [`Stasis`].
    fn flip_stasis(&self, uid: u32) {
        if let Err(e) = self.update_charter(|charter| {
            if let Some(lease) = charter.parcels.get_mut(&uid) {
                lease.stasis = true;
            }
            Ok(())
        }) {
            self.trace("decree-stasis-write-failed", format!("uid {uid}: {e}"));
        }
    }

    /// File the human escalation notice in the watched escalation inbox and,
    /// when `mayor.escalation_webhook_url` is configured, queue the optional
    /// dispatch POST (ADR-0029 cl. 4).
    async fn file_escalation(
        &self,
        id: &DecreeId,
        evidence_refs: &[TreeRelPath],
        summary: &str,
    ) -> Result<(), DecreeRefusal> {
        let notice = EscalationNotice {
            escalation_version: ESCALATION_VERSION,
            decree_id: id.clone(),
            evidence_refs: evidence_refs.to_vec(),
            recorded_unix_ms: unix_now_ms(),
            summary: summary.to_owned(),
        };
        let rendered = notice
            .render()
            .map_err(|e| DecreeRefusal::execution_failed(format!("escalation render: {e}")))?;
        let path = self.tree.manor_escalation_file(id);
        fs::create_dir_all(self.tree.manor_escalations()).map_err(|source| {
            DecreeRefusal::execution_failed(format!(
                "create {}: {source}",
                self.tree.manor_escalations().display()
            ))
        })?;
        // The notice lands in `manor/escalations/`, which the Mayor cannot
        // reach (ADR-0029 cl. 4), so the direct write is the normal path;
        // `write_atomic` still routes a `PermissionDenied` through the
        // unshare seam rather than dropping the notice on the floor.
        self.write_atomic(&path, rendered.as_bytes(), MAYOR_UID)
            .await
            .map_err(|e| DecreeRefusal::execution_failed(format!("write {path:?}: {e}")))?;
        if let Some(url) = self.config.mayor.escalation_webhook_url.clone() {
            let payload = serde_json::json!({
                "decree_id": id.render(),
                "summary": summary,
                "evidence_refs": evidence_refs.iter().map(ToString::to_string)
                    .collect::<Vec<String>>(),
            });
            let request = OutboxRequest {
                body: payload,
                // The dispatch correlation is the decree's own identity: the
                // Mayor's escalation and its webhook POST are one thing, and
                // the inbox envelope answers under the same id.
                correlation: hamlet_core::session::CorrelationId::parse(&format!(
                    "d-{}-1000.0",
                    id.seq()
                ))
                .map_err(|e| {
                    DecreeRefusal::execution_failed(format!("escalation correlation: {e}"))
                })?,
                dispatch_version: DISPATCH_VERSION,
                headers_env: BTreeMap::new(),
                method: Method::Post,
                url,
            };
            request
                .validate()
                .map_err(|e| DecreeRefusal::execution_failed(format!("escalation webhook: {e}")))?;
            let outbox = self.tree.courier_dispatch_outbox();
            let file = outbox.join(hamlet_core::dispatch::file_name(&request.correlation));
            fs::create_dir_all(&outbox).map_err(|source| {
                DecreeRefusal::execution_failed(format!("create {}: {source}", outbox.display()))
            })?;
            // The dispatch outbox is daemon-owned, but the same seam keeps
            // the escalation exactly-once checkpoint truthful if the courier
            // tree ever moves under a different owner.
            self.write_atomic(
                &file,
                request
                    .render()
                    .expect("validated request renders")
                    .as_bytes(),
                MAYOR_UID,
            )
            .await
            .map_err(|e| {
                DecreeRefusal::execution_failed(format!("write {}: {e}", file.display()))
            })?;
        }
        self.trace("decree-escalated", format!("{id}: {summary}"));
        Ok(())
    }

    // --- charter + files -------------------------------------------------

    fn load_charter(&self) -> Result<Charter, ExecutorError> {
        Charter::load(&self.tree.charter()).map_err(ExecutorError::Charter)
    }

    /// Serialize charter mutations: the daemon is the singular governor and
    /// load→mutate→save is not concurrency-safe by itself (ADR-0006 makes
    /// each *save* atomic; this makes the window exclusive).
    fn update_charter<T>(
        &self,
        f: impl FnOnce(&mut Charter) -> Result<T, ExecutorError>,
    ) -> Result<T, ExecutorError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.tree.charter();
        let mut charter = Charter::load(&path)?;
        let out = f(&mut charter)?;
        charter.validate()?;
        charter.save_atomic(&path)?;
        Ok(out)
    }

    /// The replay gate: admit the id, or classify the refusal.
    ///
    /// A refusal is a *decree* outcome, not an executor error: the Mayor
    /// reads a typed ack for it and the sequence is left unconsumed (the
    /// Mayor may re-issue the same decision under the correct sequence).
    fn admit(&self, id: &DecreeId) -> Result<DecreeAdmission, DecreeRefusal> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.tree.charter();
        let mut charter = match Charter::load(&path) {
            Ok(charter) => charter,
            Err(e) => {
                return Err(DecreeRefusal::execution_failed(format!(
                    "charter load: {e}"
                )))
            }
        };
        match charter.admit_decree(id) {
            Ok(DecreeAdmission::Admitted) => {
                charter.validate().map_err(|e| {
                    DecreeRefusal::execution_failed(format!("charter validate: {e}"))
                })?;
                charter
                    .save_atomic(&path)
                    .map_err(|e| DecreeRefusal::execution_failed(format!("charter save: {e}")))?;
                Ok(DecreeAdmission::Admitted)
            }
            Ok(replayed @ DecreeAdmission::Replayed) => Ok(replayed),
            Err(_) => {
                // Which rule refused decides the token: a sequence already
                // held by *different* content is a replay mismatch; a
                // sequence that is not the next one is out of order.
                let reason = if charter.decree_receipts.contains_key(&id.seq()) {
                    RejectReason::ReplayMismatch
                } else {
                    RejectReason::OutOfOrder
                };
                Err(DecreeRefusal::new(
                    reason,
                    format!(
                        "decree sequence {} is not admissible (charter next = {})",
                        id.seq(),
                        charter.next_decree_seq()
                    ),
                ))
            }
        }
    }

    /// Record a terminal status for an admitted decree.
    fn finish(&self, id: &DecreeId, status: DecreeStatus) -> Result<(), ExecutorError> {
        self.update_charter(|charter| {
            charter.set_decree_status(id.seq(), status)?;
            Ok(())
        })
    }

    /// Write the typed rejection ack for a decree that carries a usable
    /// identity.
    ///
    /// Deliberately **no charter write**: a decree refused *before*
    /// admission never consumed its sequence, so the charter still offers
    /// that sequence to the Mayor's next (correctly numbered) decree, and a
    /// refusal of an already-admitted sequence must never overwrite another
    /// decree's status row. The ack file plus the sled trace are the record.
    async fn reject(&self, id: &DecreeId, refusal: &DecreeRefusal) -> Result<(), ExecutorError> {
        self.write_ack(&DecreeAck::rejected(id, refusal.reason))
            .await
    }

    /// Atomically write `<decree_id>.ack` next to its decree: temp file in
    /// the same directory → fsync → rename (CONVENTIONS §3 r2). A repeated
    /// write is byte-identical, so re-acknowledging is a no-op in effect.
    ///
    /// The write goes through [`DecreeExecutor::write_atomic`], so a
    /// subuid-owned 0700 manor home (ADR-0011 cl. 4 — QA05-02) is written
    /// from inside the daemon's userns instead of being a kernel refusal
    /// that only the trace ever sees.
    async fn write_ack(&self, ack: &DecreeAck) -> Result<(), ExecutorError> {
        let id = &ack.decree_id;
        let path = self.tree.manor_decree_ack(id);
        // `render` yields the canonical bytes (sorted keys, trailing
        // newline); they are written atomically and traced compacted, so a
        // trace line never carries a newline.
        let rendered = ack.render().map_err(ExecutorError::Render)?;
        self.write_atomic(&path, rendered.as_bytes(), MAYOR_UID)
            .await?;
        let outcome = match &ack.result {
            hamlet_core::decree::AckResult::Done => "done".to_owned(),
            hamlet_core::decree::AckResult::Rejected { reason } => format!("rejected: {reason}"),
        };
        self.trace("decree-ack", format!("{id}: {outcome}"));
        Ok(())
    }

    /// Write `bytes` to `path` atomically, escalating to the unshare seam
    /// when the kernel refuses the direct write.
    ///
    /// The direct write is always tried first: it is cheaper, and on a host
    /// where the daemon *is* the owner it is the whole job. Only a
    /// `PermissionDenied` escalates — any other error is a real error and
    /// retrying it through four more processes would only hide it. Without a
    /// runtime installed there is nowhere to escalate to, so the original
    /// error is surfaced unchanged: loud, never silent (QA05-02).
    async fn write_atomic(
        &self,
        path: &Path,
        bytes: &[u8],
        owner_uid: u32,
    ) -> Result<(), ExecutorError> {
        match atomic_write(path, bytes) {
            Ok(()) => Ok(()),
            Err(source)
                if source.kind() == std::io::ErrorKind::PermissionDenied && self.has_runtime() =>
            {
                self.trace(
                    "decree-unshare-write",
                    format!(
                        "{}: direct write refused, writing from inside the daemon userns \
                         (ADR-0011 cl. 4)",
                        path.display()
                    ),
                );
                let runtime = self
                    .runtime
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .expect("checked by has_runtime");
                runtime
                    .unshare_write_atomic(path, bytes, (owner_uid, owner_uid))
                    .await
                    .map_err(|e| ExecutorError::Io {
                        context: format!(
                            "write {} directly and through podman unshare",
                            path.display()
                        ),
                        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, e),
                    })
            }
            Err(source) => Err(ExecutorError::Io {
                context: format!("write {}", path.display()),
                source,
            }),
        }
    }

    fn has_runtime(&self) -> bool {
        self.runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Rename an unidentifiable file into the hidden `.rejected/` quarantine
    /// (ADR-0026 cl. 5): still on disk for the operator, invisible to the
    /// watcher (hidden component) and to the next scan.
    ///
    /// Renaming is a write into the Mayor's own directory, so under the
    /// spec'd service user it can be refused exactly like the ack write
    /// (QA05-02). When it is, the rename goes through `podman unshare mv`
    /// (ADR-0011 cl. 4) — the same typed payload helper the ack seam uses,
    /// and never a shell.
    async fn quarantine(&self, path: &Path, name: &str) -> Result<(), ExecutorError> {
        let dir = self.tree.manor_decrees_rejected();
        if let Err(source) = fs::create_dir_all(&dir) {
            // A refused `mkdir` in the manor home means every rename below
            // will be refused too; fall through to the unshare path instead
            // of failing with an error the caller cannot act on.
            if !(source.kind() == std::io::ErrorKind::PermissionDenied && self.has_runtime()) {
                return Err(io_err(format!("create {}", dir.display()))(source));
            }
            self.unshare_quarantine_dir(&dir).await?;
        }
        for attempt in 0u32.. {
            let target = dir.join(if attempt == 0 {
                name.to_owned()
            } else {
                format!("{name}.r{attempt}")
            });
            if target.exists() {
                continue;
            }
            return match fs::rename(path, &target) {
                Ok(()) => Ok(()),
                Err(source)
                    if source.kind() == std::io::ErrorKind::PermissionDenied
                        && self.has_runtime() =>
                {
                    self.unshare_rename(path, &target).await
                }
                Err(source) => Err(io_err(format!(
                    "quarantine {} → {}",
                    path.display(),
                    target.display()
                ))(source)),
            };
        }
        unreachable!("quarantine suffix space is unbounded")
    }

    /// `podman unshare mkdir -p -- <dir>` (ADR-0011 cl. 4): create the
    /// quarantine directory inside the daemon userns when the direct
    /// `create_dir_all` is refused.
    async fn unshare_quarantine_dir(&self, dir: &Path) -> Result<(), ExecutorError> {
        let runtime = self.runtime_ref()?;
        let cmd = vec![
            "mkdir".to_owned(),
            "-p".to_owned(),
            "--".to_owned(),
            dir.to_string_lossy().into_owned(),
        ];
        let out = runtime
            .unshare_exec(&cmd)
            .await
            .map_err(|e| ExecutorError::Io {
                context: format!("create {} via podman unshare", dir.display()),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, e),
            })?;
        if out.code != 0 {
            return Err(ExecutorError::Io {
                context: format!("create {} via podman unshare", dir.display()),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("unshare mkdir exited {}", out.code),
                ),
            });
        }
        Ok(())
    }

    /// `podman unshare mv -- <src> <dest>` (ADR-0011 cl. 4).
    async fn unshare_rename(&self, src: &Path, dest: &Path) -> Result<(), ExecutorError> {
        let runtime = self.runtime_ref()?;
        let cmd = crate::podman::mv_staged_cmd(src, dest).map_err(|e| ExecutorError::Io {
            context: format!(
                "quarantine {} → {} via podman unshare",
                src.display(),
                dest.display()
            ),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, e),
        })?;
        let out = runtime
            .unshare_exec(&cmd)
            .await
            .map_err(|e| ExecutorError::Io {
                context: format!(
                    "quarantine {} → {} via podman unshare",
                    src.display(),
                    dest.display()
                ),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, e),
            })?;
        if out.code != 0 {
            return Err(ExecutorError::Io {
                context: format!(
                    "quarantine {} → {} via podman unshare",
                    src.display(),
                    dest.display()
                ),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("unshare mv exited {}", out.code),
                ),
            });
        }
        Ok(())
    }

    fn runtime_ref(&self) -> Result<Arc<dyn crate::podman::Runtime>, ExecutorError> {
        self.runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| ExecutorError::Io {
                context: "podman unshare write seam".to_owned(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "no runtime installed on this executor (ADR-0011 cl. 4)",
                ),
            })
    }

    fn trace(&self, event: &str, detail: impl Into<String>) {
        let Some(telemetry) = &self.telemetry else {
            eprintln!("hamlet-daemon: decrees {event}: {}", detail.into());
            return;
        };
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: event.to_owned(),
            detail: scrub_detail(&detail.into()),
        };
        if let Err(e) = telemetry.trace_event(
            AgentKey {
                commission: 0,
                uid: MAYOR_UID,
            },
            record,
        ) {
            eprintln!("hamlet-daemon: decrees trace {event} failed: {e}");
        }
    }
}

/// Map a parse/validate failure to its stable ack token (ADR-0026 cl. 5).
/// The order is fixed: the *first* failure in check order decides.
fn reject_reason_of(error: &DecreeError) -> RejectReason {
    match error {
        DecreeError::Oversize { .. } => RejectReason::Oversize,
        DecreeError::UnsupportedVersion(_) => RejectReason::UnsupportedVersion,
        DecreeError::InvalidUid(_) | DecreeError::InvalidCommission => RejectReason::UnknownSlot,
        DecreeError::InvalidPath { .. } | DecreeError::PathOutOfRegion { .. } => {
            RejectReason::InvalidTarget
        }
        DecreeError::InvalidText { .. }
        | DecreeError::EmptyEvidence
        | DecreeError::TooManyEvidence { .. } => RejectReason::InvalidTarget,
        DecreeError::Schema(_) | DecreeError::InvalidId(_) | DecreeError::DigestMismatch { .. } => {
            RejectReason::Malformed
        }
    }
}

/// Attempts to find a free temp name before the write is called *failed*.
const TEMP_ATTEMPTS: u32 = 256;

/// Atomic write: same-directory temp → fsync → rename (CONVENTIONS §3 r2),
/// with **ADR-0007 create-new discipline** on the temp file.
///
/// The Mayor owns exactly one writable directory in the village
/// (`keep/census/manor/home/decrees/`), and its input stream is untrusted
/// (mail to UID 1000). A temp name the daemon *predicts* and opens with
/// `create(true)` would therefore be an invitation: the Mayor pre-plants
/// `.{name}.tmp` as a symlink to `charter.json` and the daemon truncates the
/// charter through it (QA05-04, fixed here). `create_new(true)` is
/// `O_CREAT|O_EXCL`, which fails on an existing entry — **including a
/// symlink**, planted or otherwise — so the temp file is always a file this
/// call created. A planted name is never opened, only skipped: collisions
/// walk a bounded suffix sequence and exhaustion is a typed io error, never
/// a fallback to an unsafe write.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_owned());
    // Dot-prefixed temp: the watcher's hidden-component filter ignores it
    // (ADR-0019 cl. 6), so a decree ack can never look like a new event.
    for attempt in 0..TEMP_ATTEMPTS {
        let temp = dir.join(if attempt == 0 {
            format!(".{name}.tmp")
        } else {
            format!(".{name}.r{attempt}.tmp")
        });
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        return fs::rename(&temp, path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "no free temp name for {} after {TEMP_ATTEMPTS} attempts (all pre-planted)",
            path.display()
        ),
    ))
}

fn unix_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}

// ---------------------------------------------------------------------------
// Placeholder seams (no subsystem installed)
// ---------------------------------------------------------------------------

/// Provisioning seam placeholder: refuses with `deferred`, so a village
/// without a provisioner acknowledges the decree honestly instead of
/// pretending it provisioned a parcel.
pub struct DeferredProvisioner;

impl AgentProvisioner for DeferredProvisioner {
    fn provision<'a>(
        &'a self,
        request: ProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionOutcome, DecreeRefusal>> {
        Box::pin(async move {
            Err(DecreeRefusal::deferred(format!(
                "no provisioner installed; cannot provision {} for commission {}",
                request.identity, request.commission
            )))
        })
    }
}

/// Session-routing seam placeholder: refuses both queue and interrupt paths
/// with `execution_failed` (there is no supervisor to route through).
pub struct RefusingRouter;

impl SessionRouter for RefusingRouter {
    fn reassign(
        &self,
        uid: u32,
        commission: u64,
        entry: &SafeName,
        _text: &str,
    ) -> Result<(), DecreeRefusal> {
        Err(DecreeRefusal::execution_failed(format!(
            "no session router installed; cannot task uid {uid} with {commission}/{entry}"
        )))
    }

    fn steer<'a>(
        &'a self,
        uid: u32,
        _directive: &'a str,
    ) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        Box::pin(async move {
            Err(DecreeRefusal::execution_failed(format!(
                "no session router installed; cannot steer uid {uid}"
            )))
        })
    }
}

/// Hibernation seam placeholder.
pub struct RefusingStasis;

impl Stasis for RefusingStasis {
    fn park<'a>(&'a self, uid: u32) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        Box::pin(async move {
            Err(DecreeRefusal::execution_failed(format!(
                "no stasis seam installed; cannot park uid {uid}"
            )))
        })
    }
}

// ---------------------------------------------------------------------------
// Watcher attachment + recovery
// ---------------------------------------------------------------------------

/// Register the decree handler on the dispatcher: `Reaction::DecreeWrite`
/// triggers a full pass over the decree directory.
///
/// A *pass*, not a single file, is the reaction body — and that is the whole
/// determinism story: the Mayor writes with temp+rename, so a reaction can
/// fire on a partial or duplicated view, and a directory pass in
/// charter-sequence order is idempotent no matter how many times it runs.
pub fn attach(
    executor: Arc<DecreeExecutor>,
    dispatcher: &Dispatcher,
) -> tokio::task::JoinHandle<()> {
    let mut rx = dispatcher.register(HANDLER_NAME);
    tokio::spawn(async move {
        loop {
            let Some(item) = rx.recv().await else {
                break; // dispatcher gone — the daemon is shutting down
            };
            if !matches!(item.reaction, Reaction::DecreeWrite { .. }) {
                continue;
            }
            if let Err(e) = executor.scan().await {
                eprintln!("hamlet-daemon: decree pass failed: {e}");
            }
        }
    })
}

/// Bring up the decree plane: the tree watcher, the recovery pass, and the
/// `Reaction::DecreeWrite` handler.
///
/// Ordering is the whole point (the ADR-0019 precedent the mail plane set):
/// the watcher starts first (so nothing the Mayor writes is missed), the
/// **recovery pass runs next**, and only then is the reaction handler
/// attached — so a restart that left a half-executed decree finishes it
/// before any newly written decree can interleave, and no reaction races
/// recovery for the same file. [`DecreeExecutor::scan`] additionally holds
/// the execution lock, so even a pathological interleaving cannot run two
/// passes at once.
///
/// # Errors
///
/// [`WatchError`](crate::watch::WatchError) when the watcher cannot start.
pub async fn spawn_decree_plane(
    executor: Arc<DecreeExecutor>,
    dispatcher: Arc<Dispatcher>,
) -> Result<WatcherHandle, crate::watch::WatchError> {
    let handle =
        crate::watch::spawn_tree_watcher(executor.tree().clone(), Arc::clone(&dispatcher), || {})?;
    match executor.scan().await {
        Ok(report) if !report.outcomes.is_empty() => {
            eprintln!(
                "hamlet-daemon: decree recovery executed={} replayed={} rejected={} quarantined={}",
                report.executed(),
                report.replayed(),
                report.rejected(),
                report.quarantined()
            );
        }
        Ok(_) => {}
        Err(e) => eprintln!("hamlet-daemon: decree recovery failed: {e}"),
    }
    attach(executor, &dispatcher);
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Decree ack timeout → Mayor wake (ADR-0026 cl. 5)
// ---------------------------------------------------------------------------

/// One decree that has waited past `mayor.ack_timeout_ms` without its ack
/// file — the fact a `decree_ack_timeout` wake carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckTimeout {
    /// The decree awaiting its disposition.
    pub id: DecreeId,
    /// How long this process has seen the file unacknowledged. A *process*
    /// observation, never a state input (CONVENTIONS §3 r4) — like
    /// `turn_timeout_ms`, it decides when to wake, not what the village
    /// believes.
    pub waited_ms: u64,
    /// How many timeout wakes this process has already fired for this
    /// decree; the next one is `notice + 1`. Carried into the artifact body
    /// so the Mayor can tell "still waiting" from "I have been told six
    /// times already and have not answered" (ADR-0030 cl. 3).
    pub notice: u64,
}

/// Tracks how long each unacknowledged decree file has been visible and
/// fires **once per timeout**, re-notifying on `mayor.ack_retry_ms`.
///
/// The old policy was once per *process* per decree — a set that was never
/// cleared while the file lived — which meant a Mayor that ignored an ack
/// was told once and then never again until someone restarted the daemon
/// (QA05-03). That is not "once per timeout", it is "once per deployment".
///
/// The new policy separates two clocks that used to be conflated: the
/// *threshold* (`ack_timeout_ms` — has the Mayor had a fair chance?) and the
/// *cadence* (`ack_retry_ms` — how often is it worth spending tokens on the
/// same unanswered question?). Both are operator config, both are pure
/// daemon-side behavior, and the cadence default (10 min) is far above the
/// threshold (30 s) so the queue cannot be spun by one pathological decree —
/// which was ADR-0021 cl. 5's actual worry, now answered by a number an
/// operator can read instead of by process lifetime.
#[derive(Debug, Default)]
pub struct AckTimeouts {
    seen: BTreeMap<DecreeId, std::time::Instant>,
    /// When the last timeout wake fired for a decree, and how many have
    /// fired. Absent ⇒ never notified ⇒ the threshold alone gates the first.
    notified: BTreeMap<DecreeId, (std::time::Instant, u64)>,
}

impl AckTimeouts {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One poll: which decree files have been unacknowledged for longer than
    /// `timeout_ms`, ordered by sequence.
    ///
    /// Pure with respect to the tree (it only *reads* names and ack
    /// existence) and with respect to the clock (`now` is injected), so the
    /// whole policy is unit-testable without sleeping.
    pub fn poll(
        &mut self,
        tree: &HamletTree,
        timeout_ms: u64,
        retry_ms: u64,
        now: std::time::Instant,
    ) -> Vec<AckTimeout> {
        let dir = tree.manor_decrees();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut live: BTreeSet<DecreeId> = BTreeSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = hamlet_core::decree::decree_id_from_file_name(&name).ok() else {
                continue;
            };
            if tree.manor_decree_ack(&id).exists() {
                // Acknowledged: forget everything about it.
                self.seen.remove(&id);
                self.notified.remove(&id);
                continue;
            }
            live.insert(id);
        }
        // Decrees that vanished (quarantined, operator-cleaned) forget too.
        self.seen.retain(|id, _| live.contains(id));
        self.notified.retain(|id, _| live.contains(id));

        let mut out = Vec::new();
        for id in &live {
            let first = *self.seen.entry(id.clone()).or_insert(now);
            let waited = now.saturating_duration_since(first).as_millis();
            let waited_ms: u64 = waited.try_into().unwrap_or(u64::MAX);
            if waited_ms < timeout_ms {
                continue;
            }
            // Past the threshold: the Mayor gets its first word, then one
            // re-notification per `retry_ms` of continued silence.
            let slot = self.notified.entry(id.clone()).or_insert((now, 0));
            let since_last = now.saturating_duration_since(slot.0).as_millis();
            let since_last_ms: u64 = since_last.try_into().unwrap_or(u64::MAX);
            if slot.1 > 0 && since_last_ms < retry_ms {
                continue;
            }
            slot.0 = now;
            slot.1 += 1;
            out.push(AckTimeout {
                id: id.clone(),
                waited_ms,
                notice: slot.1,
            });
        }
        out
    }
}

/// Watch for unacknowledged decrees and wake the Mayor on every timeout,
/// re-notifying on `mayor.ack_retry_ms` (ADR-0026 cl. 5, ADR-0030 cl. 3;
/// Deliverable 2).
///
/// The tick is `min(ack_timeout_ms, retry_ms, 1 s)` so a decree is noticed
/// promptly while the timers themselves stay operator-configurable. The task
/// holds the Mayor's own timer only — no process, no tokens (SPEC §7 sleep).
#[must_use = "the task ends when the runtime shuts down"]
pub fn spawn_ack_timeout_watch(
    tree: HamletTree,
    engine: Arc<crate::mayor::MayorEngine>,
) -> tokio::task::JoinHandle<()> {
    let timeout_ms = engine.config().mayor.ack_timeout_ms;
    let retry_ms = engine.config().mayor.ack_retry_ms;
    let tick = std::cmp::min(timeout_ms.min(retry_ms), 1_000).max(50);
    tokio::spawn(async move {
        let mut timeouts = AckTimeouts::new();
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(tick));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            for timeout in timeouts.poll(&tree, timeout_ms, retry_ms, std::time::Instant::now()) {
                let event = crate::mayor::WakeEvent::from_decree_ack_timeout(
                    timeout.id.clone(),
                    timeout.waited_ms,
                    timeout.notice,
                );
                match event {
                    Ok(event) => {
                        if let Err(e) = engine.wake(event).await {
                            eprintln!("hamlet-daemon: mayor ack-timeout wake failed: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("hamlet-daemon: mayor ack-timeout wake refused: {e}");
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Crash-wake bridge (Deliverable 2)
// ---------------------------------------------------------------------------

/// Bridges supervision crashes to Mayor wakes, filtered by
/// `config.json` → `mayor.crash_wake` (ADR-0029 cl. 2).
///
/// The policy is evaluated **here**, not in the supervisor: supervision must
/// not know that a Mayor exists, and the Mayor must not know how the restart
/// policy is implemented.
#[derive(Clone)]
pub struct CrashWakeBridge {
    engine: Arc<crate::mayor::MayorEngine>,
}

impl CrashWakeBridge {
    /// Bridge an engine into the supervision plane.
    #[must_use]
    pub fn new(engine: Arc<crate::mayor::MayorEngine>) -> Self {
        Self { engine }
    }
}

impl CrashWake for CrashWakeBridge {
    fn crashed(&self, notice: &CrashNotice) {
        let engine = Arc::clone(&self.engine);
        let policy = engine.config().mayor.crash_wake;
        if !policy.wakes(notice.permanent) {
            return;
        }
        let notice = notice.clone();
        tokio::spawn(async move {
            match crate::mayor::WakeEvent::from_session_crash(&notice) {
                Ok(event) => {
                    if let Err(e) = engine.wake(event).await {
                        eprintln!("hamlet-daemon: mayor crash wake failed: {e}");
                    }
                }
                Err(e) => eprintln!("hamlet-daemon: mayor crash wake refused: {e}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamlet_core::decree::{DecreeAck, DECREE_VERSION};

    /// One escalation decree body stamped at `seq` (the Mayor's exact
    /// transformation).
    fn escalation(seq: u64, summary: &str) -> Decree {
        let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
        Decree::EscalateToHuman {
            decree_id: placeholder,
            decree_version: DECREE_VERSION,
            evidence_refs: vec![TreeRelPath::parse("guild/register/scribe.md").unwrap()],
            summary: summary.to_owned(),
        }
        .stamp(seq)
        .unwrap()
    }

    /// A valid tree with an empty decree directory.
    fn empty_tree() -> (tempfile::TempDir, HamletTree) {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        fs::create_dir_all(tree.manor_decrees()).unwrap();
        (tmp, tree)
    }

    /// The ack-timeout truth table (ADR-0026 cl. 5, ADR-0030 cl. 3): before
    /// the timeout — silence; past it — one wake; past it again, but before
    /// `retry_ms` — silence; past `retry_ms` — the *next* notice. An ack or
    /// a vanished file forgets everything.
    #[test]
    fn ack_timeouts_fire_once_per_timeout_and_forget() {
        let (_tmp, tree) = empty_tree();
        let id = escalation(1, "still waiting").id().clone();
        fs::write(
            tree.manor_decrees()
                .join(id.decree_file_name().unwrap().as_str()),
            "{}",
        )
        .unwrap();

        let mut timeouts = AckTimeouts::new();
        let t0 = std::time::Instant::now();
        assert!(
            timeouts.poll(&tree, 100, 10_000, t0).is_empty(),
            "before the timeout"
        );

        let late = t0 + std::time::Duration::from_millis(150);
        let fired = timeouts.poll(&tree, 100, 10_000, late);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, id);
        assert!(fired[0].waited_ms >= 100);
        assert_eq!(fired[0].notice, 1, "the first notification");

        // The retry cadence, not the process lifetime, gates the second
        // wake (QA05-03). The old policy was silent here forever.
        assert!(
            timeouts
                .poll(
                    &tree,
                    100,
                    10_000,
                    t0 + std::time::Duration::from_millis(5_000)
                )
                .is_empty(),
            "a re-notification is not due yet"
        );
        let second = timeouts.poll(
            &tree,
            100,
            10_000,
            t0 + std::time::Duration::from_millis(11_000),
        );
        assert_eq!(second.len(), 1, "one re-notification per ack_retry_ms");
        assert_eq!(second[0].notice, 2, "and it counts");
        assert_eq!(
            timeouts
                .poll(
                    &tree,
                    100,
                    10_000,
                    t0 + std::time::Duration::from_millis(21_500)
                )
                .len(),
            1,
            "still exactly one per interval, never a burst"
        );

        // The ack forgets it: a fresh timer would re-arm on the NEXT file,
        // but this id is gone from memory.
        let ack = DecreeAck::done(&id);
        fs::write(tree.manor_decree_ack(&id), ack.render().unwrap()).unwrap();
        assert!(timeouts
            .poll(
                &tree,
                100,
                10_000,
                t0 + std::time::Duration::from_millis(90_000)
            )
            .is_empty());
    }

    #[test]
    fn ack_timeouts_forget_vanished_decree_files() {
        let (_tmp, tree) = empty_tree();
        let id = escalation(2, "about to vanish").id().clone();
        let path = tree
            .manor_decrees()
            .join(id.decree_file_name().unwrap().as_str());
        fs::create_dir_all(tree.manor_decrees_rejected()).unwrap();
        fs::write(&path, "{}").unwrap();

        let mut timeouts = AckTimeouts::new();
        let t0 = std::time::Instant::now();
        // The first observation starts the timer (waited = 0): no fire.
        assert!(timeouts.poll(&tree, 100, 10_000, t0).is_empty());
        // Past the timeout: fires once.
        assert_eq!(
            timeouts
                .poll(
                    &tree,
                    100,
                    10_000,
                    t0 + std::time::Duration::from_millis(150)
                )
                .len(),
            1
        );

        fs::rename(&path, tree.manor_decrees_rejected().join("gone")).unwrap();
        assert!(timeouts
            .poll(
                &tree,
                100,
                10_000,
                t0 + std::time::Duration::from_millis(90_000)
            )
            .is_empty());
    }

    /// Every parse/validate failure maps to the stable ack token (ADR-0026
    /// cl. 5) — first failure in check order decides.
    #[test]
    fn reject_reason_of_maps_to_stable_tokens() {
        let cases: Vec<(DecreeError, RejectReason)> = vec![
            (
                DecreeError::Oversize {
                    bytes: MAX_DECREE_BYTES + 1,
                },
                RejectReason::Oversize,
            ),
            (
                DecreeError::UnsupportedVersion(2),
                RejectReason::UnsupportedVersion,
            ),
            (DecreeError::InvalidUid(999), RejectReason::UnknownSlot),
            (DecreeError::InvalidCommission, RejectReason::UnknownSlot),
            (
                DecreeError::InvalidPath {
                    path: "../x".to_owned(),
                    reason: "traversal",
                },
                RejectReason::InvalidTarget,
            ),
            (
                DecreeError::PathOutOfRegion {
                    path: "guild/x.md".to_owned(),
                },
                RejectReason::InvalidTarget,
            ),
            (DecreeError::EmptyEvidence, RejectReason::InvalidTarget),
            (
                DecreeError::TooManyEvidence { count: 11 },
                RejectReason::InvalidTarget,
            ),
            (
                DecreeError::DigestMismatch {
                    stamped: "0000000000000000".to_owned(),
                    computed: "ffffffffffffffff".to_owned(),
                },
                RejectReason::Malformed,
            ),
            (
                DecreeError::Schema("bad".to_owned()),
                RejectReason::Malformed,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(reject_reason_of(&err), want);
        }
    }

    /// `ExecutionReport` counts are derived, never accumulated.
    #[test]
    fn execution_report_counts_outcomes() {
        let report = ExecutionReport {
            outcomes: vec![
                DecreeOutcome::Executed {
                    id: escalation(1, "a").id().clone(),
                    kind: "escalate_to_human",
                },
                DecreeOutcome::Replayed {
                    id: escalation(2, "b").id().clone(),
                },
                DecreeOutcome::Rejected {
                    id: escalation(3, "c").id().clone(),
                    reason: RejectReason::OutOfOrder,
                },
            ],
        };
        assert_eq!(report.executed(), 1);
        assert_eq!(report.replayed(), 1);
        assert_eq!(report.rejected(), 1);
        assert_eq!(report.quarantined(), 0);
    }
}
