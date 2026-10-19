//! DEED dynamic-alterations → blueprint evolution (SPEC §5, §9.4, §10; ADR-0038).
//!
//! Watches DEED.md mutations per parcel ([`Reaction::DeedMutation`]), maintains an
//! incremental checksummed view of `### Dynamic Alterations`, and promotes validated
//! alterations into the parent blueprint in `guild/blueprints/` at entry-completion
//! or commission wind-down.
//!
//! # Architecture & Guarantees
//!
//! 1. **Purity & Validation**: All grammar parsing uses [`hamlet_core::promote`].
//!    Lines must conform to `- <kind> <name>[ = <version_spec>]` where
//!    `kind ∈ {pixi, nvm, cargo, apk}`. Shell metacharacters, path separators,
//!    newlines, unknown kinds, and duplicate tools are rejected with a line-numbered
//!    report mailed to the agent (`From: system`) and a sled trace.
//! 2. **Environment Floor**: Bootstrap toolchain baseline entries
//!    (`- baseline · toolchain bootstrap ...`) are treated as the environment floor
//!    and skipped rather than treated as alterations to promote.
//! 3. **Append-Only Immutability**: Promoted blueprints write a new version
//!    (`guild/blueprints/<name>.v<new_version>.toml`) and atomically update the
//!    active `<name>.toml` declaration with a version bump and provenance header
//!    (commission, uid, digest, timestamp). Existing parcels retain their bootstrap
//!    manifests.
//! 4. **Idempotence**: Re-promoting an already-merged alteration set is a no-op
//!    via content-hash and toolset matching dedupe.
//! 5. **Crash-Resilience**: All blueprint writes use atomic temp+sync+rename; no
//!    half-written blueprint can ever exist.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::blueprint::{Blueprint, BlueprintError};
use hamlet_core::charter::Charter;
use hamlet_core::decree::content_digest;
use hamlet_core::mail::{Mail, MailHeader, MailSender};
use hamlet_core::promote::{
    alterations_digest, apply_alterations, is_already_merged, parse_deed_alterations,
    parse_deed_blueprint_ref, Alteration, DeedError,
};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use tokio::task::JoinHandle;

use crate::mailroute::{MailActor, MailRouter};
use crate::telemetry::{AgentKey, Telemetry, TraceRecord};
use crate::watch::Dispatcher;

const MODE_BLUEPRINT: u32 = 0o644;
const DEFAULT_BLUEPRINT_NAME: &str = "village-alpine";

/// When promotion is triggered for a commission's parcel alterations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromotionTrigger {
    /// Promote at entry-completion (`commission:<n>:done`). Default.
    #[default]
    EntryCompletion,
    /// Promote at commission wind-down (`WindingDown` after all entries done).
    CommissionWindDown,
}

/// Outcome of evaluating and attempting to promote a parcel's DEED alterations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// Successfully merged alterations and bumped the parent blueprint version.
    Promoted {
        blueprint: SafeName,
        old_version: u32,
        new_version: u32,
        alterations_count: usize,
    },
    /// Alterations were already merged into the blueprint (content-hash / tool match dedupe).
    AlreadyMerged { blueprint: SafeName, version: u32 },
    /// The parcel has no dynamic alterations to promote (clean or baseline-only).
    NoAlterations { uid: u32 },
    /// Alteration lines failed validation against blueprint grammar (report mailed to agent + sled trace).
    Rejected { uid: u32, reason: String },
}

/// Cached view of a parcel's `### Dynamic Alterations` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDeedView {
    pub uid: u32,
    pub checksum: String,
    pub raw_section: String,
    pub alterations: Result<Vec<Alteration>, DeedError>,
    pub blueprint_ref: Option<SafeName>,
    pub commission: Option<u64>,
    pub recorded_alterations: Vec<Alteration>,
    pub has_baseline: bool,
}

/// Promotion errors encountered during DEED evaluation or blueprint write-back.
#[derive(Debug)]
pub enum PromoteError {
    Io {
        context: String,
        source: io::Error,
    },
    SymlinkRefused {
        path: PathBuf,
    },
    MissingDeed {
        uid: u32,
        path: PathBuf,
    },
    DeedValidation {
        uid: u32,
        error: DeedError,
    },
    MissingBlueprintRef {
        uid: u32,
    },
    BlueprintNotFound {
        path: PathBuf,
    },
    BlueprintInvalid {
        path: PathBuf,
        error: BlueprintError,
    },
    BlueprintVersionOverflow {
        blueprint: SafeName,
    },
    Mail {
        context: String,
        error: String,
    },
    Charter {
        context: String,
        error: String,
    },
}

impl fmt::Display for PromoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "promotion I/O {context}: {source}"),
            Self::SymlinkRefused { path } => {
                write!(f, "refusing symlink at {}", path.display())
            }
            Self::MissingDeed { uid, path } => {
                write!(f, "parcel{uid} DEED.md missing at {}", path.display())
            }
            Self::DeedValidation { uid, error } => {
                write!(f, "parcel{uid} DEED validation failed: {error}")
            }
            Self::MissingBlueprintRef { uid } => {
                write!(f, "parcel{uid} DEED.md missing blueprint reference")
            }
            Self::BlueprintNotFound { path } => {
                write!(f, "blueprint not found at {}", path.display())
            }
            Self::BlueprintInvalid { path, error } => {
                write!(f, "blueprint {} invalid: {error}", path.display())
            }
            Self::BlueprintVersionOverflow { blueprint } => {
                write!(f, "blueprint {blueprint} version overflow (at u32::MAX)")
            }
            Self::Mail { context, error } => write!(f, "promotion mail {context}: {error}"),
            Self::Charter { context, error } => write!(f, "promotion charter {context}: {error}"),
        }
    }
}

impl std::error::Error for PromoteError {}

/// Governor for DEED alteration parsing, validation, and blueprint evolution.
pub struct DeedPromoter {
    tree: HamletTree,
    telemetry: Option<Arc<Telemetry>>,
    mail: Option<Arc<MailRouter>>,
    views: Arc<Mutex<BTreeMap<u32, ParsedDeedView>>>,
    trigger: PromotionTrigger,
}

impl DeedPromoter {
    /// Create a promoter for the given tree with default [`PromotionTrigger::EntryCompletion`].
    pub fn new(tree: HamletTree) -> Self {
        Self {
            tree,
            telemetry: None,
            mail: None,
            views: Arc::new(Mutex::new(BTreeMap::new())),
            trigger: PromotionTrigger::EntryCompletion,
        }
    }

    /// Attach a telemetry store for recording sled traces.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Attach a mail router for delivering rejection reports to agents.
    #[must_use]
    pub fn with_mail(mut self, mail: Arc<MailRouter>) -> Self {
        self.mail = Some(mail);
        self
    }

    /// Configure the promotion trigger ([`PromotionTrigger::EntryCompletion`] or
    /// [`PromotionTrigger::CommissionWindDown`]).
    #[must_use]
    pub fn with_trigger(mut self, trigger: PromotionTrigger) -> Self {
        self.trigger = trigger;
        self
    }

    /// Current promotion trigger.
    #[must_use]
    pub fn trigger(&self) -> PromotionTrigger {
        self.trigger
    }

    /// Borrow the governed tree.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Inspect a parcel's DEED.md, compute its alterations checksum, parse
    /// lines against the grammar, and update the incremental cached view.
    ///
    /// Refuses symlinks per ADR-0007.
    ///
    /// # Errors
    ///
    /// Returns [`PromoteError`] if DEED.md is absent, a symlink, or unreadable.
    pub fn inspect_deed(&self, uid: u32) -> Result<ParsedDeedView, PromoteError> {
        let path = self.tree.parcel_deed(uid);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(PromoteError::SymlinkRefused { path })
            }
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(PromoteError::Io {
                    context: format!("stat {}", path.display()),
                    source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PromoteError::MissingDeed { uid, path })
            }
            Err(source) => {
                return Err(PromoteError::Io {
                    context: format!("stat {}", path.display()),
                    source,
                })
            }
        }

        let deed_text = fs::read_to_string(&path).map_err(|source| PromoteError::Io {
            context: format!("read {}", path.display()),
            source,
        })?;

        let blueprint_ref = parse_deed_blueprint_ref(&deed_text);
        let commission = self.extract_commission(uid, &deed_text);
        let raw_section = extract_raw_alterations_section(&deed_text);
        let checksum = content_digest(raw_section.as_bytes());

        let prev_view = {
            let lock = self.views.lock().unwrap_or_else(|e| e.into_inner());
            lock.get(&uid).cloned()
        };

        let has_baseline = raw_section
            .lines()
            .any(|l| l.trim().starts_with("- baseline · "));
        let mut alterations = parse_deed_alterations(&deed_text);

        let (recorded_alterations, baseline_required) = if let Some(ref prev) = prev_view {
            (prev.recorded_alterations.clone(), prev.has_baseline)
        } else {
            (Vec::new(), has_baseline)
        };

        if baseline_required && !has_baseline {
            alterations = Err(DeedError::NonAppendMutation {
                detail: "baseline entry was deleted".to_string(),
            });
        } else if let Ok(ref new_alts) = alterations {
            if !recorded_alterations.is_empty() && !new_alts.starts_with(&recorded_alterations) {
                alterations = Err(DeedError::NonAppendMutation {
                    detail: "previously recorded alterations were modified or deleted".to_string(),
                });
            }
        }

        let new_recorded = match (&alterations, &prev_view) {
            (Ok(new_alts), _) => new_alts.clone(),
            (Err(_), Some(prev)) => prev.recorded_alterations.clone(),
            (Err(_), None) => Vec::new(),
        };

        let view = ParsedDeedView {
            uid,
            checksum,
            raw_section,
            alterations,
            blueprint_ref,
            commission,
            recorded_alterations: new_recorded,
            has_baseline: baseline_required || has_baseline,
        };

        let mut lock = self.views.lock().unwrap_or_else(|e| e.into_inner());
        lock.insert(uid, view.clone());
        Ok(view)
    }

    /// Get the latest cached view for `uid` if one was observed.
    pub fn cached_view(&self, uid: u32) -> Option<ParsedDeedView> {
        let lock = self.views.lock().unwrap_or_else(|e| e.into_inner());
        lock.get(&uid).cloned()
    }

    /// Handle a [`Reaction::DeedMutation`] event by updating the cached view.
    pub fn handle_deed_mutation(&self, uid: u32) -> Result<ParsedDeedView, PromoteError> {
        self.inspect_deed(uid)
    }

    /// Promote alterations from `parcel<uid>/DEED.md` into the parent blueprint.
    ///
    /// Validates grammar: invalid lines deliver a line-numbered rejection report
    /// to `uid` via system mail and record a sled trace.
    ///
    /// Idempotent: already-merged alteration sets are a no-op.
    ///
    /// Atomic: writes version snapshot and active blueprint atomically.
    pub fn promote_parcel(&self, uid: u32) -> Result<PromotionOutcome, PromoteError> {
        let view = self.inspect_deed(uid)?;
        let commission = view.commission.unwrap_or(0);

        // 1. Check for grammar failure
        let alterations = match &view.alterations {
            Ok(alts) => alts,
            Err(deed_err) => {
                self.deliver_rejection_mail(commission, uid, deed_err)?;
                self.record_trace(
                    commission,
                    uid,
                    "deed_alteration_rejected",
                    format!("parcel{uid}: {deed_err}"),
                );
                return Ok(PromotionOutcome::Rejected {
                    uid,
                    reason: deed_err.to_string(),
                });
            }
        };

        // 2. Check for empty alterations
        if alterations.is_empty() {
            return Ok(PromotionOutcome::NoAlterations { uid });
        }

        // 3. Resolve parent blueprint
        let bp_name = view.blueprint_ref.clone().unwrap_or_else(|| {
            sanitize_name(DEFAULT_BLUEPRINT_NAME).expect("valid default blueprint name")
        });
        let bp_path = self.tree.guild_blueprint(&bp_name);

        match fs::symlink_metadata(&bp_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(PromoteError::SymlinkRefused { path: bp_path })
            }
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(PromoteError::Io {
                    context: format!("stat {}", bp_path.display()),
                    source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PromoteError::BlueprintNotFound { path: bp_path })
            }
            Err(source) => {
                return Err(PromoteError::Io {
                    context: format!("stat {}", bp_path.display()),
                    source,
                })
            }
        }

        let bp_text = fs::read_to_string(&bp_path).map_err(|source| PromoteError::Io {
            context: format!("read {}", bp_path.display()),
            source,
        })?;
        let current_bp =
            Blueprint::from_toml(&bp_text).map_err(|error| PromoteError::BlueprintInvalid {
                path: bp_path.clone(),
                error,
            })?;

        // 4. Content-hash & tool-set dedupe check
        let digest = alterations_digest(alterations);
        if is_already_merged(&current_bp, alterations)
            || bp_text.contains(&format!("digest {digest}"))
        {
            return Ok(PromotionOutcome::AlreadyMerged {
                blueprint: bp_name,
                version: current_bp.meta().version(),
            });
        }

        // 5. Merge alterations and construct evolved blueprint
        let new_bp = apply_alterations(&current_bp, alterations).map_err(|e| match e {
            BlueprintError::VersionOverflow { .. } => PromoteError::BlueprintVersionOverflow {
                blueprint: bp_name.clone(),
            },
            other => PromoteError::BlueprintInvalid {
                path: bp_path.clone(),
                error: other,
            },
        })?;

        let old_version = current_bp.meta().version();
        let new_version = new_bp.meta().version();
        let now_unix = unix_now();
        let provenance = format!(
            "Provenance: promoted from parcel{uid} on commission {commission} · digest {digest} · unix {now_unix}"
        );
        let rendered = new_bp.render_toml(Some(&provenance));

        // 6. Write version snapshot and active blueprint atomically (ADR-0006)
        let old_snap_path = self.tree.guild_blueprint_version(&bp_name, old_version);
        if !fs::symlink_metadata(&old_snap_path).is_ok_and(|m| m.is_file()) {
            let _ = write_atomic_file(&old_snap_path, bp_text.as_bytes(), MODE_BLUEPRINT);
        }

        let new_snap_path = self.tree.guild_blueprint_version(&bp_name, new_version);
        write_atomic_file(&new_snap_path, rendered.as_bytes(), MODE_BLUEPRINT)?;
        write_atomic_file(&bp_path, rendered.as_bytes(), MODE_BLUEPRINT)?;

        // 7. Record sled trace
        self.record_trace(
            commission,
            uid,
            "blueprint_promoted",
            format!(
                "{bp_name} v{old_version} -> v{new_version} ({} alterations, digest {digest})",
                alterations.len()
            ),
        );

        Ok(PromotionOutcome::Promoted {
            blueprint: bp_name,
            old_version,
            new_version,
            alterations_count: alterations.len(),
        })
    }

    /// Promote all parcels rostered on `commission` (used for commission wind-down).
    pub fn promote_commission(
        &self,
        commission: u64,
    ) -> Result<Vec<PromotionOutcome>, PromoteError> {
        let roster = self.commission_roster(commission)?;
        let mut outcomes = Vec::new();
        for uid in roster {
            outcomes.push(self.promote_parcel(uid)?);
        }
        Ok(outcomes)
    }

    fn extract_commission(&self, uid: u32, deed_text: &str) -> Option<u64> {
        for line in deed_text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("- commission: ") {
                if let Ok(c) = rest.trim().parse::<u64>() {
                    return Some(c);
                }
            }
        }
        let charter_path = self.tree.charter();
        if let Ok(charter) = Charter::load(&charter_path) {
            if let Some(slot) = charter.slots.get(&uid) {
                return slot.commission;
            }
        }
        None
    }

    fn commission_roster(&self, commission: u64) -> Result<Vec<u32>, PromoteError> {
        let charter_path = self.tree.charter();
        let charter = Charter::load(&charter_path).map_err(|e| PromoteError::Charter {
            context: format!("load {}", charter_path.display()),
            error: e.to_string(),
        })?;
        let Some(record) = charter.commissions.get(&commission) else {
            return Ok(Vec::new());
        };
        Ok(record.roster.iter().copied().collect())
    }

    fn deliver_rejection_mail(
        &self,
        commission: u64,
        uid: u32,
        deed_err: &DeedError,
    ) -> Result<(), PromoteError> {
        let subject = format!("commission:{commission}:deed:rejected");
        let body = format!(
            "# DEED Alteration Rejected\n\n\
             Dynamic alterations in parcel{uid} DEED.md failed validation on commission {commission}.\n\n\
             Error: {deed_err}\n\n\
             Alterations must conform to the blueprint [tools] grammar:\n\
               - <kind> <name>[ = <version_spec>]\n\n\
             Allowed kinds: pixi, nvm, cargo, apk.\n\
             Names and versions must not contain shell metacharacters, path separators, or spaces.\n"
        );
        let date = format!("unix:{}", unix_now());

        if let Some(router) = &self.mail {
            router
                .send(MailActor::System, uid, &subject, &body, Some(&date))
                .map_err(|e| PromoteError::Mail {
                    context: format!("send mail to {uid}"),
                    error: e.to_string(),
                })?;
            if !router.has_handler() {
                if let Ok(recipient) = sanitize_name(&uid.to_string()) {
                    let _ = router.route_recipient(&recipient);
                }
            }
            return Ok(());
        }

        // Direct write fallback when router is not wired
        let from = MailSender::Name(sanitize_name("system").expect("valid system name"));
        let mail = Mail {
            header: MailHeader {
                from,
                to: uid,
                subject,
                date,
            },
            body,
        };
        let rendered = mail.render();
        if let Ok(recipient) = sanitize_name(&uid.to_string()) {
            let dir = self.tree.courier_mail_recipient(&recipient);
            let fname = format!("{:012}-system-deed-rejected.mail", unix_now_ms());
            let path = dir.join(fname);
            let _ = fs::create_dir_all(&dir);
            let _ = write_atomic_file(&path, rendered.as_bytes(), 0o600);
        }
        Ok(())
    }

    fn record_trace(&self, commission: u64, uid: u32, event: &str, detail: String) {
        if let Some(telemetry) = &self.telemetry {
            let record = TraceRecord {
                at_unix_ms: unix_now_ms(),
                event: event.to_owned(),
                detail,
            };
            if let Err(e) = telemetry.trace_event(AgentKey { commission, uid }, record) {
                eprintln!("hamlet-daemon: promotion trace {event} failed: {e}");
            }
        }
    }
}

/// Spawn the DEED watcher plane: registers `"deed"` on the dispatcher and
/// updates the promoter's parsed view on every [`Reaction::DeedMutation`].
pub fn spawn_deed_plane(promoter: Arc<DeedPromoter>, dispatcher: &Dispatcher) -> JoinHandle<()> {
    let mut rx = dispatcher.register("deed");
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if let Reaction::DeedMutation { uid } = item.reaction {
                let _ = promoter.handle_deed_mutation(uid);
            }
        }
    })
}

fn extract_raw_alterations_section(deed: &str) -> String {
    const HEADING: &str = "### Dynamic Alterations";
    let mut in_section = false;
    let mut out = String::new();
    for line in deed.lines() {
        let trimmed = line.trim();
        if in_section {
            if trimmed.starts_with('#') {
                break;
            }
            out.push_str(line);
            out.push('\n');
        } else if trimmed == HEADING {
            in_section = true;
        }
    }
    out
}

fn write_atomic_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), PromoteError> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(PromoteError::SymlinkRefused {
                path: path.to_path_buf(),
            });
        }
    }
    let parent = path.parent().ok_or_else(|| PromoteError::Io {
        context: format!("no parent directory for {}", path.display()),
        source: io::Error::new(io::ErrorKind::InvalidInput, "no parent"),
    })?;
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_owned());
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp_path);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp_path)
        .map_err(|source| PromoteError::Io {
            context: format!("create temp {}", tmp_path.display()),
            source,
        })?;
    file.write_all(bytes).map_err(|source| PromoteError::Io {
        context: format!("write temp {}", tmp_path.display()),
        source,
    })?;
    file.sync_all().map_err(|source| PromoteError::Io {
        context: format!("fsync temp {}", tmp_path.display()),
        source,
    })?;
    drop(file);

    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode)).map_err(|source| {
        PromoteError::Io {
            context: format!("chmod {:04o} {}", mode, tmp_path.display()),
            source,
        }
    })?;
    fs::rename(&tmp_path, path).map_err(|source| PromoteError::Io {
        context: format!("rename {} -> {}", tmp_path.display(), path.display()),
        source,
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
