//! Freight POSIX ACL sandboxes: audit, quarantine, promotion-read
//! (SPEC §4 "Freight ACLs", §8.3; ADR-0021).
//!
//! Agents create `/var/freight/<dir>` inside their containers and secure it
//! with `setfacl` for a recipient UID (the `acl` apk tool ships in the
//! default blueprint). The daemon **audits** every sandbox on its creation
//! event ([`hamlet_core::watch::Reaction::FreightDir`], ADR-0019 cl. 5) and
//! on every plane start (restart-resumability, CONVENTIONS §3 r6):
//!
//! - owner must be a valid charter slot UID;
//! - no `other::rw` (access *or* default ACL) — no world-writable sandboxes;
//! - no grant to a UID outside the commission-scoped roster;
//! - no named *group* grants (group membership is not charter state).
//!
//! Violations are pure-data verdicts from [`hamlet_core::freight::audit`]
//! over `getfacl -p -n -c` listings obtained through `podman unshare`
//! (ADR-0011 cl. 4: the daemon is root-equivalent inside its own userns,
//! so agent-owned 0700 sandboxes are always inspectable). A violation
//! writes a **quarantine flag** (`.quarantine/<name>.json`, daemon-owned,
//! mode 0444 — readable evidence for agents, never agent-writable) plus a
//! sled trace; repeated violations by one owner escalate through the
//! [`MayorWake`] hook (Sprint 05 consumes it).
//!
//! The daemon **never relaxes ACLs on an agent's behalf** (SPEC §4): the
//! only host-side ACL writes are the freight-root creator grants
//! ([`FreightGuard::ensure_root_acl`]/[`FreightGuard::grant_slot`],
//! ADR-0021 cl. 2) that let slots create sandboxes at all. The Sprint 06
//! promotion path reads through [`FreightGuard::promotion_read`] — a
//! host-side capability (userns root) that leaves every agent-side ACL
//! untouched, and refuses to follow symlinks (SPEC §4 traversal invariant).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::charter::{Charter, CharterError, CommissionStatus};
use hamlet_core::freight::{
    audit as audit_listing, parse_getfacl, AclGrant, AuditPolicy, Qualifier, Violation,
    ViolationKind,
};
use hamlet_core::mail::is_slot_uid;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use serde::{Deserialize, Serialize};

use crate::podman::{self, ExecOutput, PodmanError, Runtime};
use crate::supervise::Supervisor;
use crate::telemetry::{AgentKey, TelemetryError, TraceRecord};
use crate::watch::{Dispatched, Dispatcher};

/// Quarantine flags per owner that trip the Mayor-wake escalation
/// (ADR-0021 cl. 5). Derived from the flag files on disk — file-as-truth,
/// survives restarts; the in-process dedupe only prevents re-firing within
/// one daemon lifetime.
pub const ESCALATION_THRESHOLD: usize = 3;

/// Schema version of the quarantine flag file.
pub const FLAG_VERSION: u32 = 1;

/// Mode of the quarantine flag dir: daemon-owned, world-readable — agents
/// see WHY a sandbox was quarantined but can never edit or unlink evidence
/// (the dir grants no write to any slot).
pub const MODE_QUARANTINE_DIR: u32 = 0o755;

/// Mode of a quarantine flag file: read-only to everyone but the daemon
/// writer (ADR-0021 cl. 5: "daemon-written, ro to agents").
pub const MODE_QUARANTINE_FLAG: u32 = 0o444;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed freight failures. Audit *violations* are outcomes
/// ([`AuditOutcome::Flagged`]), not errors: an error means the sandbox
/// stays unaudited and the next event or restart re-audits it.
#[derive(Debug)]
pub enum FreightError {
    /// Filesystem failure with context.
    Io {
        /// What was attempted.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
    /// podman seam failure (spawn, argv rejection, exit 125).
    Podman(PodmanError),
    /// A UID outside the slot range (1000 Mayor–2000) was asked for a root
    /// grant (refused before any side effect — SPEC §4 UID policy).
    NotASlot {
        /// The refused UID.
        uid: u32,
    },
    /// A host tool (`getfacl`/`setfacl`/`cat`) exited non-zero.
    ToolFailure {
        /// The tool that failed.
        tool: String,
        /// Exit code.
        code: i32,
        /// Captured stderr.
        stderr: String,
    },
    /// Tool output was not valid UTF-8 (corruption is never tolerated).
    Utf8 {
        /// Which output.
        context: String,
        /// Underlying error.
        source: std::str::Utf8Error,
    },
    /// The `getfacl` listing violated the grammar (hostile/corrupt input).
    Parse(hamlet_core::freight::FreightError),
    /// Charter load failure while deriving the roster.
    Charter(CharterError),
    /// Telemetry sink failure (traces degrade, never corrupt — the audit
    /// verdict itself is already persisted in the flag file).
    Telemetry(TelemetryError),
    /// A quarantine flag file failed to parse (daemon-written; corruption
    /// surfaces, never silently recovered).
    FlagCorrupt {
        /// Offending flag path.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },
    /// The freight entry is not a real directory (file/symlink at sandbox
    /// level — a traversal/escape shape, never promoted or audited as a
    /// sandbox).
    NotADirectory {
        /// Offending path.
        path: PathBuf,
    },
    /// A promotion-read target (or its sandbox) is a symlink: refused
    /// outright (SPEC §4: no escape via promoted filenames).
    SymlinkRefused {
        /// Offending path.
        path: PathBuf,
    },
    /// A promotion-read target exists but is not a regular file.
    NotAFile {
        /// Offending path.
        path: PathBuf,
    },
}

impl fmt::Display for FreightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Podman(e) => write!(f, "podman: {e}"),
            Self::NotASlot { uid } => write!(f, "uid {uid} is not a slot uid (1000–2000)"),
            Self::ToolFailure { tool, code, stderr } => {
                write!(f, "{tool} exited {code}: {stderr}")
            }
            Self::Utf8 { context, source } => write!(f, "{context}: invalid UTF-8: {source}"),
            Self::Parse(e) => write!(f, "getfacl listing: {e}"),
            Self::Charter(e) => write!(f, "charter: {e}"),
            Self::Telemetry(e) => write!(f, "telemetry: {e}"),
            Self::FlagCorrupt { path, reason } => {
                write!(f, "corrupt quarantine flag {}: {reason}", path.display())
            }
            Self::NotADirectory { path } => write!(f, "not a directory: {}", path.display()),
            Self::SymlinkRefused { path } => write!(f, "symlink refused: {}", path.display()),
            Self::NotAFile { path } => write!(f, "not a regular file: {}", path.display()),
        }
    }
}

impl std::error::Error for FreightError {}

// ---------------------------------------------------------------------------
// Quarantine flag (file-as-truth evidence, ADR-0021 cl. 5)
// ---------------------------------------------------------------------------

/// One violation as recorded in a flag file (`kind` = the stable
/// [`ViolationKind::kind_str`] identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagViolation {
    /// Stable violation identity (never renumbered).
    pub kind: String,
    /// Render-stable detail (uids/entry text only — never artifact
    /// content, CONVENTIONS §6).
    pub detail: String,
}

/// Daemon-written quarantine evidence for one violating sandbox. JSON,
/// sorted-field canonical rendering, mode 0444 under
/// `courier/freight/.quarantine/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineFlag {
    /// Flag schema version.
    pub flag_version: u32,
    /// Sandbox dir name under `courier/freight/`.
    pub dir: String,
    /// Owner UID as seen inside the userns (0 when the entry was not a
    /// real directory and no listing owner exists).
    pub owner: u32,
    /// Every violation the audit found, in audit order.
    pub violations: Vec<FlagViolation>,
}

impl QuarantineFlag {
    /// Build the flag for one audit verdict.
    #[must_use]
    pub fn new(dir: &SafeName, owner: u32, violations: &[Violation]) -> Self {
        Self {
            flag_version: FLAG_VERSION,
            dir: dir.as_str().to_owned(),
            owner,
            violations: violations
                .iter()
                .map(|v| FlagViolation {
                    kind: v.kind.kind_str().to_owned(),
                    detail: v.detail.clone(),
                })
                .collect(),
        }
    }

    /// Canonical rendering (deterministic — struct field order, pretty).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out =
            serde_json::to_string_pretty(self).expect("QuarantineFlag serialization is infallible");
        out.push('\n');
        out
    }

    /// Parse one flag file body.
    ///
    /// # Errors
    ///
    /// serde rejection (unknown/missing fields, wrong version) as a string
    /// reason for [`FreightError::FlagCorrupt`].
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let flag: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if flag.flag_version != FLAG_VERSION {
            return Err(format!(
                "unsupported flag_version {} (want {FLAG_VERSION})",
                flag.flag_version
            ));
        }
        Ok(flag)
    }
}

// ---------------------------------------------------------------------------
// Mayor wake hook (Sprint 05 consumes)
// ---------------------------------------------------------------------------

/// Repeated-violation escalation payload for the Mayor wake event
/// (SPEC §7: the daemon wakes the Mayor with a pristine single-turn
/// context packet — Sprint 05 builds the packet; this is the trigger).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreightEscalation {
    /// The offending sandbox owner (charter slot UID when known).
    pub owner: u32,
    /// Every quarantined dir attributed to that owner, sorted.
    pub quarantined: Vec<String>,
}

/// The hook seam Sprint 05 plugs the Mayor wake loop into.
pub trait MayorWake: Send + Sync {
    /// Record/deliver one escalation. Implementations must not block the
    /// audit task on Mayor liveness (the hook is fire-and-forget).
    fn wake(&self, escalation: &FreightEscalation);
}

/// Default sink until Sprint 05: one structured stderr line, nothing else.
#[derive(Debug, Default)]
pub struct StderrWake;

impl MayorWake for StderrWake {
    fn wake(&self, escalation: &FreightEscalation) {
        eprintln!(
            "hamlet-daemon: MAYOR-WAKE freight escalation owner={} quarantined={:?}",
            escalation.owner, escalation.quarantined
        );
    }
}

/// In-process recording sink — the test/inspection seam (like
/// [`crate::podman::FakeRuntime`] for podman).
#[derive(Debug, Default)]
pub struct RecordingWake {
    events: Mutex<Vec<FreightEscalation>>,
}

impl RecordingWake {
    /// A fresh empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every escalation recorded so far, in order.
    #[must_use]
    pub fn events(&self) -> Vec<FreightEscalation> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl MayorWake for RecordingWake {
    fn wake(&self, escalation: &FreightEscalation) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(escalation.clone());
    }
}

// ---------------------------------------------------------------------------
// Outcomes & reports
// ---------------------------------------------------------------------------

/// The result of auditing one freight entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The entry vanished before the audit (idempotent no-op — reactions
    /// are hints, ADR-0019 cl. 3).
    Missing,
    /// The listing satisfied every rule; no flag written. A pre-existing
    /// flag (earlier verdict) is left untouched — flags are evidence,
    /// cleared only by the Mayor/operator path (Sprint 05+).
    Clean {
        /// Owner UID from the listing.
        owner: u32,
    },
    /// Violations found: flag written (0444), sled traced; `escalated`
    /// marks that this audit tripped the [`MayorWake`] hook.
    Flagged {
        /// Owner UID recorded in the flag (0 for non-directory entries).
        owner: u32,
        /// The violations, in audit order.
        violations: Vec<Violation>,
        /// Whether the escalation threshold tripped on this audit.
        escalated: bool,
    },
}

/// What [`FreightGuard::ensure_root_acl`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootAclReport {
    /// Slot UIDs freshly granted `u:<uid>:rwx` on the freight root.
    pub granted: Vec<u32>,
    /// Slot UIDs whose grant already existed.
    pub present: usize,
}

/// What one [`FreightGuard::audit_all`] pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuditReport {
    /// Visible entries considered.
    pub scanned: usize,
    /// Entries that audited [`AuditOutcome::Clean`].
    pub clean: usize,
    /// Entries that audited [`AuditOutcome::Flagged`].
    pub flagged: usize,
    /// Owners escalated through the wake hook during the pass.
    pub escalated: usize,
    /// Visible entries whose names fail the §4 sanitizer (unauditable —
    /// logged, never flagged: the flag path itself needs a SafeName).
    pub skipped: usize,
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// The village freight governor: root-ACL provisioning, sandbox audits,
/// quarantine evidence, escalation hook, and the Sprint 06 promotion-read
/// capability. All methods take `&self`; one instance per village (the
/// singular governor, CONVENTIONS §2).
pub struct FreightGuard {
    tree: HamletTree,
    supervisor: Arc<Supervisor>,
    runtime: Arc<dyn Runtime>,
    wake: Arc<dyn MayorWake>,
    /// Owners already escalated in this process lifetime (dedupe — the
    /// threshold count itself is file-as-truth via the flag dir).
    fired: Mutex<BTreeSet<u32>>,
    /// Temp-file serial for atomic flag writes.
    tmp_serial: AtomicU64,
}

impl FreightGuard {
    /// Guard over the supervisor's village tree (charter + telemetry come
    /// from the supervisor — one governor seam), the podman seam and the
    /// Mayor-wake hook.
    #[must_use]
    pub fn new(
        supervisor: Arc<Supervisor>,
        runtime: Arc<dyn Runtime>,
        wake: Arc<dyn MayorWake>,
    ) -> Self {
        Self {
            tree: supervisor.tree().clone(),
            supervisor,
            runtime,
            wake,
            fired: Mutex::new(BTreeSet::new()),
            tmp_serial: AtomicU64::new(0),
        }
    }

    /// The village tree this guard audits.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    // --- roster ------------------------------------------------------------

    /// Charter-derived audit policy (ADR-0021 cl. 3): owners = every
    /// charter slot; grantees = the commission-scoped roster (Mayor +
    /// slots bound to an **Open** commission — slots of closed commissions
    /// and idle slots are not shareable).
    ///
    /// # Errors
    ///
    /// [`FreightError::Charter`] when `charter.json` fails to load.
    pub fn policy(&self) -> Result<AuditPolicy, FreightError> {
        let charter = self.charter()?;
        let mut owners = BTreeSet::new();
        let mut grantees = BTreeSet::new();
        for (uid, slot) in &charter.slots {
            owners.insert(*uid);
            let open = match slot.commission {
                Some(commission) => {
                    charter.commissions.get(&commission) == Some(&CommissionStatus::Open)
                }
                None => false,
            };
            if *uid == MAYOR_UID || open {
                grantees.insert(*uid);
            }
        }
        Ok(AuditPolicy { owners, grantees })
    }

    fn charter(&self) -> Result<Charter, FreightError> {
        Charter::load(&self.tree.charter()).map_err(FreightError::Charter)
    }

    // --- root ACL provisioning (the only host-side ACL writes) -----------

    /// Idempotent freight-root provisioning (ADR-0021 cl. 2): sticky
    /// daemon-owned root at [`crate::hostperm::MODE_FREIGHT`], the hidden
    /// `.quarantine/` evidence dir, and a `u:<uid>:rwx` root grant for
    /// every charter slot so slots can create sandboxes at all. Never
    /// touches any sandbox ACL (SPEC §4: the daemon never relaxes ACLs on
    /// an agent's behalf — this is the *root*, not a sandbox).
    ///
    /// # Errors
    ///
    /// I/O, podman, or listing failures.
    pub async fn ensure_root_acl(&self) -> Result<RootAclReport, FreightError> {
        let root = self.tree.courier_freight();
        let quarantine = self.tree.courier_freight_quarantine();
        fs::create_dir_all(&root).map_err(|source| FreightError::Io {
            context: format!("create {}", root.display()),
            source,
        })?;
        fs::create_dir_all(&quarantine).map_err(|source| FreightError::Io {
            context: format!("create {}", quarantine.display()),
            source,
        })?;
        set_mode(&quarantine, MODE_QUARANTINE_DIR)?;
        set_mode(&root, crate::hostperm::MODE_FREIGHT)?;

        let policy = self.policy()?;
        let listing = self.listing(&root).await?;
        let named: BTreeSet<u32> = listing
            .access
            .iter()
            .filter_map(|entry| match entry.qualifier {
                Qualifier::User(Some(uid)) => Some(uid),
                _ => None,
            })
            .collect();
        let missing: Vec<u32> = policy.owners.difference(&named).copied().collect();
        let present = policy.owners.len() - missing.len();
        if missing.is_empty() {
            return Ok(RootAclReport {
                granted: Vec::new(),
                present,
            });
        }
        let mods: Vec<String> = missing
            .iter()
            .map(|uid| AclGrant::root_access(*uid).token())
            .collect();
        let cmd = podman::setfacl_cmd(&mods, &root).map_err(FreightError::Podman)?;
        self.unshare(&cmd, "setfacl").await?;
        Ok(RootAclReport {
            granted: missing,
            present,
        })
    }

    /// Grant one freshly allocated slot root access (the provisioning
    /// path — Sprint 07 census calls this after `allocate_slot`, mirroring
    /// `MailRouter::ensure_spool_dirs` advice). Idempotent. The Mayor slot
    /// (1000) is grantable — the manor container mounts `/var/freight/`
    /// too (SPEC §4 mount table).
    ///
    /// # Errors
    ///
    /// [`FreightError::NotASlot`] outside the slot table; podman/tool
    /// failures typed.
    pub async fn grant_slot(&self, uid: u32) -> Result<(), FreightError> {
        if !is_slot_uid(uid) {
            return Err(FreightError::NotASlot { uid });
        }
        let root = self.tree.courier_freight();
        let mods = [AclGrant::root_access(uid).token()];
        let cmd = podman::setfacl_cmd(&mods, &root).map_err(FreightError::Podman)?;
        self.unshare(&cmd, "setfacl").await?;
        Ok(())
    }

    // --- audit -------------------------------------------------------------

    /// Audit one freight sandbox (ADR-0021 cl. 3–5). Idempotent: a
    /// vanished entry is [`AuditOutcome::Missing`]; a clean entry writes
    /// nothing; violations write/refresh the quarantine flag, trace to
    /// sled, and may trip the escalation hook.
    ///
    /// # Errors
    ///
    /// Listing/charter/I/O failures — the sandbox then stays unaudited and
    /// the next event or [`Self::audit_all`] retries it.
    pub async fn audit(&self, name: &SafeName) -> Result<AuditOutcome, FreightError> {
        let dir = self.tree.courier_freight_sandbox(name);
        let meta = match fs::symlink_metadata(&dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(AuditOutcome::Missing),
            Err(source) => {
                return Err(FreightError::Io {
                    context: format!("lstat {}", dir.display()),
                    source,
                })
            }
        };
        // Non-directories (files, symlinks) at sandbox level are a
        // traversal/escape shape: flagged without a listing (lstat never
        // follows, so the flag records what the entry *is*).
        let (owner, violations) = if meta.file_type().is_dir() {
            let listing = self.listing(&dir).await?;
            let policy = self.policy()?;
            (listing.owner, audit_listing(&listing, &policy))
        } else {
            (
                0,
                vec![Violation {
                    kind: ViolationKind::NotADirectory,
                    detail: format!(
                        "{} is a {}, not a sandbox directory",
                        name,
                        if meta.file_type().is_symlink() {
                            "symlink"
                        } else {
                            "file"
                        }
                    ),
                }],
            )
        };
        if violations.is_empty() {
            return Ok(AuditOutcome::Clean { owner });
        }
        self.write_flag(name, owner, &violations)?;
        self.trace_violation(name, owner, &violations);
        let escalated = self.escalate_if_repeated(owner)?;
        Ok(AuditOutcome::Flagged {
            owner,
            violations,
            escalated,
        })
    }

    /// Re-audit every visible freight entry in sorted order (plane-startup
    /// hook): converges flags for dirs created while the daemon was down —
    /// restart-resumability (CONVENTIONS §3 r6), reactions-are-hints.
    ///
    /// # Errors
    ///
    /// The first hard scan/audit failure (partial convergence is still
    /// persisted — flags of earlier entries survive).
    pub async fn audit_all(&self) -> Result<AuditReport, FreightError> {
        let mut report = AuditReport::default();
        for entry in sorted_visible(&self.tree.courier_freight())? {
            match sanitize_name(&entry) {
                Ok(name) => {
                    report.scanned += 1;
                    match self.audit(&name).await? {
                        AuditOutcome::Missing => report.scanned -= 1,
                        AuditOutcome::Clean { .. } => report.clean += 1,
                        AuditOutcome::Flagged { escalated, .. } => {
                            report.flagged += 1;
                            if escalated {
                                report.escalated += 1;
                            }
                        }
                    }
                }
                Err(e) => {
                    report.skipped += 1;
                    eprintln!(
                        "hamlet-daemon: freight entry {entry:?} fails the §4 sanitizer ({e}); \
                         unauditable (promotion would refuse it too)"
                    );
                }
            }
        }
        Ok(report)
    }

    /// Every quarantine flag on disk, keyed by sandbox dir name (sorted).
    ///
    /// # Errors
    ///
    /// [`FreightError::FlagCorrupt`] on any unreadable flag — corruption
    /// surfaces, never silently skipped.
    pub fn quarantine_flags(&self) -> Result<BTreeMap<String, QuarantineFlag>, FreightError> {
        let dir = self.tree.courier_freight_quarantine();
        let mut flags = BTreeMap::new();
        for entry in sorted_visible(&dir)? {
            let path = dir.join(&entry);
            let bytes = fs::read(&path).map_err(|source| FreightError::Io {
                context: format!("read {}", path.display()),
                source,
            })?;
            let flag =
                QuarantineFlag::parse(&bytes).map_err(|reason| FreightError::FlagCorrupt {
                    path: path.clone(),
                    reason,
                })?;
            flags.insert(flag.dir.clone(), flag);
        }
        Ok(flags)
    }

    // --- promotion-read (Sprint 06 helper) ---------------------------------

    /// Daemon read-access to any freight artifact **regardless of ACL**
    /// (ADR-0021 cl. 6): `podman unshare cat` — userns-root host-side
    /// capability, zero relaxation of the agent-side ACLs. Refuses
    /// symlinked sandboxes and symlinked/non-regular targets outright
    /// (SPEC §4 traversal invariant) *before* any read.
    ///
    /// # Errors
    ///
    /// [`FreightError::SymlinkRefused`], [`FreightError::NotADirectory`],
    /// [`FreightError::NotAFile`], or tool/podman failures.
    pub async fn promotion_read(
        &self,
        sandbox: &SafeName,
        file: &SafeName,
    ) -> Result<Vec<u8>, FreightError> {
        let dir = self.tree.courier_freight_sandbox(sandbox);
        let meta = fs::symlink_metadata(&dir).map_err(|source| FreightError::Io {
            context: format!("lstat {}", dir.display()),
            source,
        })?;
        if meta.file_type().is_symlink() {
            return Err(FreightError::SymlinkRefused { path: dir });
        }
        if !meta.file_type().is_dir() {
            return Err(FreightError::NotADirectory { path: dir });
        }
        let target = dir.join(file.as_str());
        let meta = fs::symlink_metadata(&target).map_err(|source| FreightError::Io {
            context: format!("lstat {}", target.display()),
            source,
        })?;
        if meta.file_type().is_symlink() {
            return Err(FreightError::SymlinkRefused { path: target });
        }
        if !meta.file_type().is_file() {
            return Err(FreightError::NotAFile { path: target });
        }
        let cmd = podman::cat_cmd(&target).map_err(FreightError::Podman)?;
        let out = self.unshare(&cmd, "cat").await?;
        Ok(out.stdout)
    }

    // --- plane wiring -------------------------------------------------------

    /// Register the freight handler on the dispatcher and spawn its
    /// sequential audit task: [`Reaction::FreightDir`] events audit one
    /// sandbox at a time (bounded concurrency = 1, like the mail router).
    /// Other reaction variants are ignored — the shared dispatcher fans
    /// out to every plane.
    pub fn attach(self: &Arc<Self>, dispatcher: &Dispatcher) -> tokio::task::JoinHandle<()> {
        let mut rx = dispatcher.register("freight");
        let guard = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(Dispatched { reaction, .. }) = rx.recv().await {
                let Reaction::FreightDir { name } = reaction else {
                    continue;
                };
                match guard.audit(&name).await {
                    Ok(AuditOutcome::Missing | AuditOutcome::Clean { .. }) => {}
                    Ok(AuditOutcome::Flagged {
                        owner,
                        violations,
                        escalated,
                    }) => {
                        eprintln!(
                            "hamlet-daemon: freight sandbox {} quarantined (owner={owner}, \
                             violations={}, escalated={escalated})",
                            name,
                            violations.len()
                        );
                    }
                    Err(e) => {
                        // Unaudited now; the next event or plane-start
                        // audit_all retries (never dropped).
                        eprintln!("hamlet-daemon: freight audit {name} failed: {e}");
                    }
                }
            }
        })
    }

    // --- internals ----------------------------------------------------------

    /// One `podman unshare` tool call with non-zero-exit enforcement.
    async fn unshare(&self, cmd: &[String], tool: &str) -> Result<ExecOutput, FreightError> {
        let out = self
            .runtime
            .unshare_exec(cmd)
            .await
            .map_err(FreightError::Podman)?;
        if out.code != 0 {
            return Err(FreightError::ToolFailure {
                tool: tool.to_owned(),
                code: out.code,
                stderr: out.stderr_lossy(),
            });
        }
        Ok(out)
    }

    /// `getfacl -p -n -c` through the userns (numeric in-ns UIDs) → parsed
    /// listing. Strict UTF-8: corrupt output is an error, never lossy.
    async fn listing(&self, path: &Path) -> Result<hamlet_core::freight::AclListing, FreightError> {
        let cmd = podman::getfacl_cmd(path).map_err(FreightError::Podman)?;
        let out = self.unshare(&cmd, "getfacl").await?;
        let text = std::str::from_utf8(&out.stdout).map_err(|source| FreightError::Utf8 {
            context: format!("getfacl {}", path.display()),
            source,
        })?;
        parse_getfacl(text).map_err(FreightError::Parse)
    }

    /// Atomic flag write (ADR-0006 discipline): hidden temp in the
    /// quarantine dir → fsync → rename → chmod 0444 → dir fsync.
    fn write_flag(
        &self,
        name: &SafeName,
        owner: u32,
        violations: &[Violation],
    ) -> Result<(), FreightError> {
        let dir = self.tree.courier_freight_quarantine();
        fs::create_dir_all(&dir).map_err(|source| FreightError::Io {
            context: format!("create {}", dir.display()),
            source,
        })?;
        set_mode(&dir, MODE_QUARANTINE_DIR)?;
        let final_path = self.tree.courier_freight_quarantine_flag(name);
        let serial = self.tmp_serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".flag-tmp-{serial}"));
        let flag = QuarantineFlag::new(name, owner, violations);
        {
            let mut out = fs::File::create(&tmp).map_err(|source| FreightError::Io {
                context: format!("create {}", tmp.display()),
                source,
            })?;
            use std::io::Write;
            out.write_all(flag.render().as_bytes())
                .and_then(|()| out.sync_all())
                .map_err(|source| FreightError::Io {
                    context: format!("write {}", tmp.display()),
                    source,
                })?;
        }
        fs::rename(&tmp, &final_path).map_err(|source| FreightError::Io {
            context: format!("rename {}", final_path.display()),
            source,
        })?;
        set_mode(&final_path, MODE_QUARANTINE_FLAG)?;
        fsync_dir(&dir)?;
        Ok(())
    }

    /// Count flags per owner (file-as-truth — the just-written flag is
    /// already on disk); trip the wake hook once per owner per process
    /// lifetime at/over [`ESCALATION_THRESHOLD`].
    fn escalate_if_repeated(&self, owner: u32) -> Result<bool, FreightError> {
        let flags = self.quarantine_flags()?;
        let mut by_owner: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for (dir, flag) in &flags {
            by_owner.entry(flag.owner).or_default().push(dir.clone());
        }
        let quarantined = by_owner.remove(&owner).unwrap_or_default();
        if quarantined.len() < ESCALATION_THRESHOLD {
            return Ok(false);
        }
        let mut fired = self.fired.lock().unwrap_or_else(|e| e.into_inner());
        if !fired.insert(owner) {
            return Ok(false); // already escalated this lifetime
        }
        drop(fired);
        let escalation = FreightEscalation { owner, quarantined };
        self.trace(
            owner,
            "freight-escalation",
            format!(
                "owner {owner} hit {ESCALATION_THRESHOLD}+ quarantined sandboxes: {:?}",
                escalation.quarantined
            ),
        );
        self.wake.wake(&escalation);
        Ok(true)
    }

    /// Best-effort sled trace under the owner's agent key (telemetry loss
    /// degrades, never corrupts — ADR-0005 cl. 6).
    fn trace_violation(&self, name: &SafeName, owner: u32, violations: &[Violation]) {
        let kinds: Vec<&str> = violations.iter().map(|v| v.kind.kind_str()).collect();
        self.trace(
            owner,
            "freight-violation",
            format!("dir={name} owner={owner} kinds={kinds:?}"),
        );
    }

    fn trace(&self, owner: u32, event: &str, detail: String) {
        let commission = self
            .charter()
            .ok()
            .and_then(|charter| charter.slots.get(&owner).and_then(|slot| slot.commission))
            .unwrap_or(0);
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: event.to_owned(),
            detail,
        };
        if let Err(e) = self.supervisor.telemetry().trace_event(
            AgentKey {
                commission,
                uid: owner,
            },
            record,
        ) {
            eprintln!("hamlet-daemon: freight trace {event} failed: {e}");
        }
    }
}

/// Bring up the freight plane: root-ACL provisioning + full convergence
/// audit **before** the event handler attaches, so plane-start state is
/// always consistent; then register on the shared dispatcher. The daemon
/// owns ONE tree watcher (the mail plane's or main's) — this plane only
/// registers a handler (ADR-0019's dispatcher extension point).
///
/// # Errors
///
/// Root-ACL provisioning or startup-audit failures (the plane does not
/// come up half-baked).
pub async fn spawn_freight_plane(
    supervisor: Arc<Supervisor>,
    runtime: Arc<dyn Runtime>,
    wake: Arc<dyn MayorWake>,
    dispatcher: &Dispatcher,
) -> Result<(Arc<FreightGuard>, tokio::task::JoinHandle<()>), FreightError> {
    let guard = Arc::new(FreightGuard::new(supervisor, runtime, wake));
    let root = guard.ensure_root_acl().await?;
    if !root.granted.is_empty() {
        eprintln!(
            "hamlet-daemon: freight root grants added for {:?}",
            root.granted
        );
    }
    let report = guard.audit_all().await?;
    if report.scanned > 0 {
        eprintln!(
            "hamlet-daemon: freight startup audit scanned={} clean={} flagged={} skipped={}",
            report.scanned, report.clean, report.flagged, report.skipped
        );
    }
    let handle = guard.attach(dispatcher);
    Ok((guard, handle))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sorted visible (non-hidden) entry names of one directory; a missing
/// directory scans empty. Deterministic order (CONVENTIONS §3 r1).
fn sorted_visible(dir: &Path) -> Result<Vec<String>, FreightError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(FreightError::Io {
                context: format!("scan {}", dir.display()),
                source,
            })
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| FreightError::Io {
            context: format!("scan {}", dir.display()),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), FreightError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| FreightError::Io {
        context: format!("chmod {mode:o} {}", path.display()),
        source,
    })
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), FreightError> {
    Ok(())
}

fn fsync_dir(dir: &Path) -> Result<(), FreightError> {
    fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|source| FreightError::Io {
            context: format!("fsync {}", dir.display()),
            source,
        })
}

/// Lineage-only timestamp (CONVENTIONS §3 r4: telemetry, never state).
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}
