//! Census execution (SPEC §10): parcel provisioning — Sprint 02 step 03.
//! Hibernation routine + quick awakening land here in Sprint 07.
//!
//! [`Provisioner::provision_parcel`] is the single flow that turns a
//! register identity + commission into a live, enforcing lease:
//!
//! 1. allocate the permanent UID slot (charter, Sprint 01 API — exhaustion
//!    is a typed hard error, never a wrap);
//! 2. create `keep/census/parcel<uid>/` with `home/` and the stub `TITLE.md`
//!    (first chain-of-title entry: trigger + unix timestamp as *lineage
//!    data only*, CONVENTIONS §3 rule 4), `DEED.md` (with an empty
//!    `### Dynamic Alterations` block, SPEC §9.4 parse target) and
//!    `PROPRIETOR.md` (seeded from the immutable register brief, SPEC §5);
//! 3. apply the ADR-0011 host permission table ([`crate::hostperm`]) so the
//!    mount table is genuinely enforcing *before* any container starts;
//! 4. resolve the derived parcel image and `run_detached` with
//!    [`hamlet_core::mount::MountSet::for_agent`] (SPEC §4) via the [`Runtime`] seam
//!    (ADR-0012: `[meta].base_image` is the PARENT, the apk tools live in a
//!    local derived layer);
//! 5. **bootstrap the home** ([`crate::bootstrap`], SPEC §5: the daemon
//!    executes blueprints at provisioning) — toolchains from pinned,
//!    digest-verified artifacts, the env layer into `$HOME/.profile`, the
//!    baseline entry into DEED.md — all BEFORE the first agent session;
//! 6. record the lease in the charter and `save_atomic` (CONVENTIONS §3).
//!
//! **Rollback:** every step's side effects are tracked; any failure unwinds
//! cleanly — created dirs removed (outermost-created first, pre-existing
//! state never touched), a started container `rm --force`d, and the charter
//! on disk never sees the failed lease (it is saved last, atomically).
//! Rollback failures are surfaced as [`ProvisionError::RollbackFailed`]
//! carrying both the cause and every unwind failure.
//!
//! **Crash convergence:** re-provisioning the same identity after a crash
//! adopts intact state (slot allocation is idempotent per identity; stub
//! files are `create_new`-written and adopted — never clobbered; ADR-0007
//! lstat discipline) instead of failing on `AlreadyExists`. A *live*
//! container from the crashed run makes `run_detached` fail on the taken
//! name; re-attaching live sessions is Sprint 03 supervision territory —
//! rollback deliberately never removes containers it did not start.
//!
//! Rollback also survives its own permission model: paths this run chowned
//! to a subuid slot (home, spool — 0700, unreadable to the daemon's host
//! user) are re-owned to uid 0 inside the daemon's userns (`podman unshare
//! chown 0:0`, ADR-0011 cl. 4) before removal.
//!
//! The daemon is the singular governor: callers must serialize provisioning
//! against other charter mutations (charter load→mutate→save is not
//! concurrency-safe by itself; ADR-0006 only makes each *save* atomic).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{AgentRef, Charter, CharterError, ParcelLease};
use hamlet_core::config::{Config, Secrets};
use hamlet_core::decree::content_digest;
use hamlet_core::sanitize::{sanitize_name, SafeName, SanitizeError};
use hamlet_core::session::SessionStatus;
use hamlet_core::slots::{SlotError, SlotStatus, FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};
use hamlet_core::stasis::{
    BackupManifest, ManifestEntry, ManifestEntryType, ManifestError, PatronDoc, PatronError,
    PatronFrontmatter, TitleDoc, TitleEntry, TitleEntryKind, TitleError, MANIFEST_SCHEMA_VERSION,
    MAX_PROPRIETOR_BODY_BYTES, PATRON_SCHEMA_VERSION,
};
use hamlet_core::tree::HamletTree;

use crate::bootstrap::{
    BootstrapError, BootstrapReport, BootstrapTarget, Bootstrapper, ImageOutcome,
};
use crate::decrees::{DecreeRefusal, Stasis};
use crate::hostperm::{set_mode, HostPermError, HostPermissions, MODE_PARCEL_DIR, MODE_PARCEL_DOC};
use crate::podman::{BoxFuture, ContainerState, PodmanError, RunSpec, Runtime};
use crate::supervise::{scrub_detail, ParcelRef, Supervisor};
use crate::telemetry::{AgentKey, Telemetry, TraceRecord};

/// Failures along the provisioning flow, including rollback reporting.
#[derive(Debug)]
pub enum ProvisionError {
    /// Filesystem failure with the step that failed.
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// UID slot policy violation — exhaustion is a hard error (SPEC §4).
    Slot(SlotError),
    /// Charter load/validate/save failure (charter on disk stays intact).
    Charter(CharterError),
    /// Podman invocation failure (argv + stderr per ADR-0010 cl. 5).
    Podman(PodmanError),
    /// Host permission model failure ([`crate::hostperm`]).
    HostPerm(HostPermError),
    /// Image resolution or toolchain bootstrap failure
    /// ([`crate::bootstrap`]; SPEC §5 blueprint execution).
    Bootstrap(BootstrapError),
    /// No immutable register brief exists for the identity (SPEC §5: the
    /// register entry + blueprint fully define a deployable agent — without
    /// a brief there is nothing to seed PROPRIETOR.md from). Raised in
    /// pre-flight, before ANY side effect.
    MissingRegisterBrief {
        /// Identity without a brief.
        identity: SafeName,
        /// Expected brief path (`guild/register/<name>.md`).
        path: PathBuf,
    },
    /// A path that must be a regular file is a symlink/dir/other — refused,
    /// never followed or adopted (ADR-0007 duty on lazy per-identity dests).
    NotRegularFile {
        /// The offending path.
        path: PathBuf,
    },
    /// The flow failed AND the unwind was incomplete: `cause` is the
    /// original error, `failures` lists every rollback step that failed.
    /// Operator intervention required; nothing is silently orphaned.
    RollbackFailed {
        /// Display of the original provisioning error.
        cause: String,
        /// Each failed unwind action, in attempt order.
        failures: Vec<String>,
    },
}

impl fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "provision: {context}: {source}"),
            Self::Slot(e) => write!(f, "provision: slot: {e}"),
            Self::Charter(e) => write!(f, "provision: charter: {e}"),
            Self::Podman(e) => write!(f, "provision: {e}"),
            Self::HostPerm(e) => write!(f, "provision: {e}"),
            Self::Bootstrap(e) => write!(f, "provision: {e}"),
            Self::MissingRegisterBrief { identity, path } => write!(
                f,
                "provision: no register brief for '{identity}' at {} (SPEC §5 — refusing to seed PROPRIETOR.md)",
                path.display()
            ),
            Self::NotRegularFile { path } => write!(
                f,
                "provision: {} must be a regular file; refusing to follow or adopt (ADR-0007)",
                path.display()
            ),
            Self::RollbackFailed { cause, failures } => {
                write!(f, "provision failed: {cause}; ROLLBACK INCOMPLETE: ")?;
                for failure in failures {
                    write!(f, "[{failure}] ")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ProvisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Slot(e) => Some(e),
            Self::Charter(e) => Some(e),
            Self::Podman(e) => Some(e),
            Self::HostPerm(e) => Some(e),
            Self::Bootstrap(e) => Some(e),
            Self::RollbackFailed { .. }
            | Self::MissingRegisterBrief { .. }
            | Self::NotRegularFile { .. } => None,
        }
    }
}

impl From<PodmanError> for ProvisionError {
    fn from(e: PodmanError) -> Self {
        Self::Podman(e)
    }
}

impl From<HostPermError> for ProvisionError {
    fn from(e: HostPermError) -> Self {
        Self::HostPerm(e)
    }
}

impl From<BootstrapError> for ProvisionError {
    fn from(e: BootstrapError) -> Self {
        Self::Bootstrap(e)
    }
}

fn io_err(context: &str) -> impl FnOnce(std::io::Error) -> ProvisionError + '_ {
    move |source| ProvisionError::Io {
        context: context.to_string(),
        source,
    }
}

/// Current unix time in seconds — **lineage/telemetry data only** (TITLE.md
/// stamps); never a state input (CONVENTIONS §3 rule 4).
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Current UTC ISO-8601 string (e.g. `2026-10-19T01:03:40Z`).
#[must_use]
pub fn now_iso8601() -> String {
    format_epoch_iso8601(unix_now())
}

/// Format POSIX epoch seconds as ISO-8601 UTC string.
#[must_use]
pub fn format_epoch_iso8601(secs: u64) -> String {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;

    let z = days as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_month = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_year = if m_month <= 2 { y + 1 } else { y };

    format!("{y_year:04}-{m_month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

// ---------------------------------------------------------------------------
// Stub documents (pure builders — golden-tested)
// ---------------------------------------------------------------------------

/// First chain-of-title entry (SPEC §10): trigger + timestamp as lineage.
#[must_use]
pub fn title_stub(identity: &SafeName, uid: u32, commission: u64, unix_ts: u64) -> String {
    let ts_str = format_epoch_iso8601(unix_ts);
    format!(
        "# TITLE — chain of title · parcel{uid}\n\
         \n\
         - [{ts_str}] trigger commission={commission} prompt=\"provision_parcel(identity=\\\"{identity}\\\", commission={commission}) unix {unix_ts}\"\n\
         - [{ts_str}] provision uid={uid} archetype={identity} blueprint=default version=1\n"
    )
}

/// First chain-of-title document seeded from blueprint metadata (SPEC §10, ADR-0039).
#[must_use]
pub fn title_stub_with_blueprint(
    identity: &SafeName,
    uid: u32,
    commission: u64,
    blueprint: &Blueprint,
    unix_ts: u64,
) -> String {
    let ts_str = format_epoch_iso8601(unix_ts);
    let meta = blueprint.meta();
    format!(
        "# TITLE — chain of title · parcel{uid}\n\
         \n\
         - [{ts_str}] trigger commission={commission} prompt=\"provision_parcel(identity=\\\"{identity}\\\", commission={commission}) unix {unix_ts}\"\n\
         - [{ts_str}] provision uid={uid} archetype={identity} blueprint={} version={}\n",
        meta.name(),
        meta.version(),
    )
}

/// Initial TITLE document for Manor (SPEC §10: Manor exempt from hibernation).
#[must_use]
pub fn manor_title_stub(blueprint: &Blueprint, unix_ts: u64) -> String {
    let ts_str = format_epoch_iso8601(unix_ts);
    let meta = blueprint.meta();
    format!(
        "# TITLE — manor\n\
         \n\
         - [{ts_str}] provision uid=1000 archetype=manor blueprint={} version={}\n",
        meta.name(),
        meta.version(),
    )
}

/// Environment record with the empty `### Dynamic Alterations` block the
/// Sprint 06 convergence parser consumes (SPEC §9.4).
#[must_use]
pub fn deed_stub(identity: &SafeName, uid: u32, commission: u64, blueprint: &Blueprint) -> String {
    let meta = blueprint.meta();
    format!(
        "# DEED — environment record · parcel{uid}\n\
         \n\
         - identity: {identity}\n\
         - uid slot: {uid}\n\
         - commission: {commission}\n\
         - blueprint: {name} v{version} · base image {image}\n\
         \n\
         Tool installs and environment mutations are recorded here; prompt/behavior/\n\
         memory changes belong in PROPRIETOR.md (SPEC §10 separation rule). At\n\
         commission end the block below is parsed and promoted into the parent\n\
         blueprint (SPEC §9.4) — daemon-performed only.\n\
         \n\
         ### Dynamic Alterations\n",
        name = meta.name(),
        version = meta.version(),
        image = meta.base_image(),
    )
}

/// Intelligence record seeded from the immutable register brief (SPEC §5:
/// the register is never edited; runtime mutation is recorded here).
#[must_use]
pub fn proprietor_stub(identity: &SafeName, uid: u32, brief: &str) -> String {
    format!(
        "# PROPRIETOR — {identity} · parcel{uid}\n\
         \n\
         Mutable agent record (SPEC §10 intelligence layer), seeded from the\n\
         immutable register brief below. Runtime behavior/memory mutations are\n\
         appended in the final section — the register itself is never edited\n\
         (SPEC §5).\n\
         \n\
         ## Register brief (immutable seed)\n\
         \n\
         {brief}\n\
         \n\
         ## Runtime mutations\n"
    )
}

// ---------------------------------------------------------------------------
// ADR-0007-disciplined file helpers
// ---------------------------------------------------------------------------

/// Write a stub document: absent → `create_new` (never follows a planted
/// symlink); existing regular file → ADOPTED untouched (crash convergence),
/// mode canonicalized to 0644; anything else → typed refusal.
fn write_stub(path: &Path, content: &str) -> Result<(), ProvisionError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => set_mode(path, MODE_PARCEL_DOC)?,
        Ok(_) => return Err(ProvisionError::NotRegularFile { path: path.into() }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(io_err(&format!("create {}", path.display())))?;
            file.write_all(content.as_bytes())
                .map_err(io_err(&format!("write {}", path.display())))?;
            set_mode(path, MODE_PARCEL_DOC)?;
        }
        Err(source) => {
            return Err(ProvisionError::Io {
                context: format!("stat {}", path.display()),
                source,
            })
        }
    }
    Ok(())
}

/// Read the register brief with the same lstat discipline: absent →
/// [`ProvisionError::MissingRegisterBrief`]; non-regular → typed refusal.
fn read_register_brief(tree: &HamletTree, identity: &SafeName) -> Result<String, ProvisionError> {
    let path = tree.register_brief(identity);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(ProvisionError::NotRegularFile { path }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProvisionError::MissingRegisterBrief {
                identity: identity.clone(),
                path,
            })
        }
        Err(source) => {
            return Err(ProvisionError::Io {
                context: format!("stat {}", path.display()),
                source,
            })
        }
    }
    fs::read_to_string(&path).map_err(io_err(&format!("read brief {}", path.display())))
}

// ---------------------------------------------------------------------------
// Rollback tracking
// ---------------------------------------------------------------------------

/// Everything a failed provisioning run must unwind: directories *we*
/// created (pre-existing state is never touched) and the container name, if
/// `run_detached` succeeded before the failure.
#[derive(Debug, Default)]
struct Undo {
    created_dirs: Vec<PathBuf>,
    /// Per-UID paths THIS run created and handed to a slot via unshare
    /// chown — re-owned to uid 0 (= the daemon's host user) before removal,
    /// because their 0700 subuid ownership would otherwise lock the daemon
    /// out of its own rollback (ADR-0011 consequence).
    chowned: Vec<PathBuf>,
    container: Option<String>,
}

impl Undo {
    /// Unwind in reverse order of creation; returns one message per failed
    /// action (empty = clean rollback).
    async fn execute(self, runtime: &dyn Runtime) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(name) = &self.container {
            if let Err(e) = runtime.remove(name, true).await {
                failures.push(format!("podman rm --force {name}: {e}"));
            }
        }
        if !self.chowned.is_empty() {
            // Best-effort: when podman itself is the failure (spawn gone)
            // the paths were never chowned and removal works regardless;
            // anything that still blocks removal is reported below.
            let _ = runtime.unshare_chown(0, 0, &self.chowned).await;
        }
        for dir in self.created_dirs.iter().rev() {
            if let Err(e) = fs::remove_dir_all(dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    failures.push(format!("remove {}: {e}", dir.display()));
                }
            }
        }
        failures
    }
}

// ---------------------------------------------------------------------------
// Provisioner
// ---------------------------------------------------------------------------

/// Everything one provisioning run produced: the recorded lease plus what the
/// image resolution and blueprint bootstrap did. The bootstrap report is the
/// pkgcache hit instrumentation (Sprint 02 step 04) — telemetry and the
/// guarded suites assert "zero downloads on re-provision" from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    /// The charter lease for the parcel.
    pub lease: ParcelLease,
    /// The derived image the container runs (ADR-0012 cl. 2).
    pub image: ImageOutcome,
    /// What [`crate::bootstrap`] installed / verified.
    pub bootstrap: BootstrapReport,
}

/// The provisioning governor over one village tree. Cheap to construct;
/// the daemon holds one alongside its [`Runtime`].
pub struct Provisioner {
    tree: HamletTree,
    config: Config,
    runtime: Arc<dyn Runtime>,
}

impl Provisioner {
    /// Provisioner over a tree, engine config (resource flags, podman bin)
    /// and the runtime seam (real podman or [`crate::podman::FakeRuntime`]).
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

    /// Provision (or re-converge) the parcel lease for `identity` on
    /// `commission`, bootstrapped from `blueprint`. Returns the recorded
    /// [`ParcelLease`] (uid = the lease's charter key; the slot is the
    /// identity's permanent one).
    ///
    /// Thin wrapper over [`Provisioner::provision_parcel_with_report`] for
    /// callers that only need the lease.
    ///
    /// # Errors
    ///
    /// [`ProvisionError`] — pre-flight (missing brief), slot exhaustion,
    /// io/permission/podman/charter failures. On any error after the first
    /// side effect, all tracked effects are unwound before returning; an
    /// incomplete unwind is reported as [`ProvisionError::RollbackFailed`].
    pub async fn provision_parcel(
        &self,
        identity: &SafeName,
        commission: u64,
        blueprint: &Blueprint,
    ) -> Result<ParcelLease, ProvisionError> {
        Ok(self
            .provision_parcel_with_report(identity, commission, blueprint)
            .await?
            .lease)
    }

    /// Like [`Provisioner::provision_parcel`], but also returns the derived
    /// image outcome and the bootstrap report (SPEC §5: the daemon executes
    /// the blueprint at provisioning; the report is the evidence).
    ///
    /// # Errors
    ///
    /// As [`Provisioner::provision_parcel`].
    pub async fn provision_parcel_with_report(
        &self,
        identity: &SafeName,
        commission: u64,
        blueprint: &Blueprint,
    ) -> Result<Provisioned, ProvisionError> {
        // --- pre-flight: no side effects before these pass ---------------
        let brief = read_register_brief(&self.tree, identity)?;
        let mut charter = Charter::load(&self.tree.charter()).map_err(ProvisionError::Charter)?;
        // Step 1: permanent slot (idempotent per identity; exhaustion hard).
        let uid = charter
            .allocate_slot(identity.clone(), Some(commission))
            .map_err(ProvisionError::Slot)?;

        let mut undo = Undo::default();
        let outcome = self
            .provision_steps(
                identity,
                commission,
                blueprint,
                uid,
                &brief,
                &mut charter,
                &mut undo,
            )
            .await;
        match outcome {
            Ok(provisioned) => Ok(provisioned),
            Err(err) => {
                let failures = undo.execute(self.runtime.as_ref()).await;
                if failures.is_empty() {
                    Err(err)
                } else {
                    Err(ProvisionError::RollbackFailed {
                        cause: err.to_string(),
                        failures,
                    })
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn provision_steps(
        &self,
        identity: &SafeName,
        commission: u64,
        blueprint: &Blueprint,
        uid: u32,
        brief: &str,
        charter: &mut Charter,
        undo: &mut Undo,
    ) -> Result<Provisioned, ProvisionError> {
        // --- step 2: parcel dirs + stub documents ------------------------
        let parcel = self.tree.parcel(uid);
        let parcel_existed = path_exists(&parcel);
        fs::create_dir_all(&parcel).map_err(io_err(&format!("create {}", parcel.display())))?;
        if !parcel_existed {
            undo.created_dirs.push(parcel.clone());
        }
        set_mode(&parcel, MODE_PARCEL_DIR)?;

        let home = self.tree.parcel_home(uid);
        let home_existed = path_exists(&home);
        let home_created = !home_existed;
        fs::create_dir_all(&home).map_err(io_err(&format!("create {}", home.display())))?;
        if parcel_existed && home_created {
            // The parcel dir is pre-existing (untracked) — track home on its
            // own. When the parcel dir is tracked, its removal takes home
            // with it, so no separate entry is needed.
            undo.created_dirs.push(home.clone());
        }

        // The commission git dir is a bind-mount source (SPEC §4) — podman
        // requires it to exist; create + track like the rest.
        let commission_dir = self.tree.commission(commission);
        let git_dir = self.tree.commission_git(commission);
        let commission_existed = path_exists(&commission_dir);
        let git_existed = path_exists(&git_dir);
        fs::create_dir_all(&git_dir).map_err(io_err(&format!("create {}", git_dir.display())))?;
        if !commission_existed {
            undo.created_dirs.push(commission_dir);
        } else if !git_existed {
            undo.created_dirs.push(git_dir);
        }

        // Per-UID mail spool dir (modes/ownership in step 3).
        let spool_name = sanitize_name(&uid.to_string()).map_err(|e| ProvisionError::Io {
            context: format!("spool name for uid {uid}: {e}"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "spool name"),
        })?;
        let spool = self.tree.courier_mail_recipient(&spool_name);
        let spool_existed = path_exists(&spool);
        if !spool_existed {
            fs::create_dir(&spool).map_err(io_err(&format!("create {}", spool.display())))?;
            undo.created_dirs.push(spool.clone());
        }

        // Stub documents (create_new / adopt-regular / refuse-other).
        let title_content =
            title_stub_with_blueprint(identity, uid, commission, blueprint, unix_now());
        write_stub(&self.tree.parcel_title(uid), &title_content)?;
        if let Ok(doc) = TitleDoc::parse(&title_content) {
            let key = (self.tree.root().to_path_buf(), uid);
            let mut cache = title_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(key, (doc.entries.clone(), doc.content_digest()));
        }
        write_stub(
            &self.tree.parcel_deed(uid),
            &deed_stub(identity, uid, commission, blueprint),
        )?;
        write_stub(
            &self.tree.parcel_proprietor(uid),
            &proprietor_stub(identity, uid, brief),
        )?;

        // --- step 3: host permission model (ADR-0011) --------------------
        // Belt (ro mounts) AND braces (host perms) before anything runs.
        let perms = HostPermissions::new(self.runtime.as_ref());
        if home_created {
            // Rollback must re-own (unshare chown 0:0) any home THIS run
            // handed to the slot before removal, whatever the tracking
            // granularity above (removal recurses through the 0700 dir).
            undo.chowned.push(home);
        }
        perms.apply_parcel_home(&self.tree, uid).await?;
        if !spool_existed {
            undo.chowned.push(spool);
        }
        perms.apply_mail_spool(&self.tree, uid).await?;
        perms.apply_shared(&self.tree, commission).await?;

        // --- step 4: derived image + container ----------------------------
        // ADR-0012: `[meta].base_image` is the parent; the apk tools and the
        // bootstrap floor live in a derived local image, built once per
        // (blueprint version, floor version, Containerfile fingerprint).
        let bootstrapper = Bootstrapper::new(&self.tree, self.runtime.as_ref())?;
        let image = bootstrapper.ensure_image(blueprint).await?;
        let spec = RunSpec::for_agent(identity, uid, commission, blueprint, &self.config)?
            .with_image(image.tag.clone());
        let _container_id = self.runtime.run_detached(&spec, &self.tree).await?;
        undo.container = Some(identity.to_string());

        // --- step 5: toolchain bootstrap BEFORE the first session ----------
        // SPEC §5: the daemon executes the blueprint at provisioning. The
        // home is 0700 subuid-owned (ADR-0011), so every write happens
        // in-container as the slot uid — never from the host side.
        let target = BootstrapTarget::new(identity.clone(), uid)?;
        let bootstrap = bootstrapper.bootstrap_home(&target, blueprint).await?;
        crate::ledger::seed_agent_git_home(self.runtime.as_ref(), identity, uid).await?;

        // --- step 6: record the lease (atomic; last, so disk never sees a
        // lease whose container failed or whose home failed to bootstrap) ---
        // Legacy auto-registration (ADR-0036 cl. 2): idempotent Draft record.
        charter
            .ensure_commission(commission)
            .map_err(ProvisionError::Charter)?;
        if charter
            .commissions
            .get(&commission)
            .is_some_and(|r| r.state().is_live())
        {
            charter
                .assign_uid(commission, uid)
                .map_err(ProvisionError::Charter)?;
        }
        charter
            .agents
            .entry(identity.clone())
            .or_insert(AgentRef { slot: uid });
        let lease = ParcelLease {
            identity: identity.clone(),
            stasis: false,
        };
        charter.parcels.insert(uid, lease.clone());
        charter.validate().map_err(ProvisionError::Charter)?;
        charter
            .save_atomic(&self.tree.charter())
            .map_err(ProvisionError::Charter)?;
        Ok(Provisioned {
            lease,
            image,
            bootstrap,
        })
    }

    /// Read census state across all parcels and manor for this village.
    pub fn list_census(
        &self,
        telemetry: Option<&Telemetry>,
    ) -> Result<Vec<CensusParcelView>, CensusError> {
        list_census(&self.tree, telemetry)
    }
}

// ---------------------------------------------------------------------------
// Census View & Listing API (SPEC §10)
// ---------------------------------------------------------------------------

/// Detailed view of one parcel lease / census record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusParcelView {
    /// Slot UID (1000 for manor, 1001..=2000 for agents).
    pub uid: u32,
    /// Registered archetype or identity.
    pub identity: SafeName,
    /// Commission ID if assigned.
    pub commission: Option<u64>,
    /// Commission slug if resolved from charter.
    pub commission_slug: Option<SafeName>,
    /// Current state: "live", "idle", "crashed", "stasis", "stopped", or "exempt" (manor).
    pub state: String,
    /// True if parked in stasis (hibernated).
    pub stasis: bool,
    /// Number of sessions run / recorded.
    pub session_count: u32,
    /// Activity / telemetry counters from sled.
    pub counters: CensusCounters,
    /// Last event token from TITLE.md (e.g. "provision", "session", "crash", "steer", "promotion").
    pub last_event: Option<String>,
    /// Last event timestamp string from TITLE.md.
    pub last_event_ts: Option<String>,
    /// TITLE.md FNV-1a content digest for tamper verification.
    pub title_digest: String,
}

/// Sled telemetry counters for one census parcel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CensusCounters {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens consumed.
    pub output_tokens: u64,
    /// API cost in micro-USD.
    pub cost_micro_usd: u64,
    /// System event traces recorded.
    pub traces: u64,
}

/// Failures encountered when reading, writing, or listing census records.
#[derive(Debug)]
pub enum CensusError {
    /// Filesystem failure with context.
    Io {
        context: String,
        source: std::io::Error,
    },
    /// Charter load/save/validate failure.
    Charter(CharterError),
    /// TITLE.md document parse or append failure.
    Title(TitleError),
    /// Name sanitization failure.
    Sanitize(SanitizeError),
    /// Symlink refused under ADR-0007.
    NotRegularFile { path: PathBuf },
    /// Non-append or corrupt tamper evidence detected on read.
    Tamper { uid: u32, detail: String },
}

impl fmt::Display for CensusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "census: {context}: {source}"),
            Self::Charter(e) => write!(f, "census: charter: {e}"),
            Self::Title(e) => write!(f, "census: title: {e}"),
            Self::Sanitize(e) => write!(f, "census: sanitize: {e}"),
            Self::NotRegularFile { path } => {
                write!(
                    f,
                    "census: {} is not a regular file (ADR-0007)",
                    path.display()
                )
            }
            Self::Tamper { uid, detail } => {
                write!(f, "census: parcel {uid} TITLE.md tampered: {detail}")
            }
        }
    }
}

impl std::error::Error for CensusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Charter(e) => Some(e),
            Self::Title(e) => Some(e),
            Self::Sanitize(e) => Some(e),
            Self::NotRegularFile { .. } | Self::Tamper { .. } => None,
        }
    }
}

impl From<CharterError> for CensusError {
    fn from(e: CharterError) -> Self {
        Self::Charter(e)
    }
}

impl From<TitleError> for CensusError {
    fn from(e: TitleError) -> Self {
        Self::Title(e)
    }
}

impl From<SanitizeError> for CensusError {
    fn from(e: SanitizeError) -> Self {
        Self::Sanitize(e)
    }
}

// ---------------------------------------------------------------------------
// TITLE.md Writer & Tamper Detection (SPEC §10, ADR-0039)
// ---------------------------------------------------------------------------

/// Hook for waking the Mayor when census / TITLE tamper evidence is detected.
pub trait CensusWake: Send + Sync {
    /// Fired when an agent-side or external mutation violates the append-only invariant.
    fn title_tampered(&self, uid: u32, identity: &SafeName, detail: &str);
}

/// Recording wake for unit and integration tests.
#[derive(Debug, Default)]
pub struct RecordingCensusWake {
    events: Mutex<Vec<(u32, SafeName, String)>>,
}

impl RecordingCensusWake {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn events(&self) -> Vec<(u32, SafeName, String)> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl CensusWake for RecordingCensusWake {
    fn title_tampered(&self, uid: u32, identity: &SafeName, detail: &str) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push((
            uid,
            identity.clone(),
            detail.to_string(),
        ));
    }
}

/// Production bridge delivering census tamper events into a [`crate::mayor::MayorEngine`].
#[derive(Clone)]
pub struct CensusWakeBridge {
    engine: Arc<crate::mayor::MayorEngine>,
}

impl CensusWakeBridge {
    #[must_use]
    pub fn new(engine: Arc<crate::mayor::MayorEngine>) -> Self {
        Self { engine }
    }
}

impl CensusWake for CensusWakeBridge {
    fn title_tampered(&self, uid: u32, identity: &SafeName, detail: &str) {
        let engine = Arc::clone(&self.engine);
        let identity = identity.clone();
        let detail = detail.to_string();
        tokio::spawn(async move {
            match crate::mayor::WakeEvent::from_title_tampered(uid, identity, detail) {
                Ok(event) => {
                    if let Err(e) = engine.wake(event).await {
                        eprintln!("hamlet-daemon: mayor title-tamper wake failed: {e}");
                    }
                }
                Err(e) => eprintln!("hamlet-daemon: build title-tamper event failed: {e}"),
            }
        });
    }
}

pub type TitleCacheMap = BTreeMap<(PathBuf, u32), (Vec<TitleEntry>, String)>;

static TITLE_CACHE: OnceLock<Mutex<TitleCacheMap>> = OnceLock::new();
static GLOBAL_WAKE: OnceLock<Mutex<Option<Arc<dyn CensusWake>>>> = OnceLock::new();
static GLOBAL_TELEMETRY: OnceLock<Mutex<Option<Arc<Telemetry>>>> = OnceLock::new();

pub fn title_cache() -> &'static Mutex<TitleCacheMap> {
    TITLE_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn global_wake() -> &'static Mutex<Option<Arc<dyn CensusWake>>> {
    GLOBAL_WAKE.get_or_init(|| Mutex::new(None))
}

fn global_telemetry() -> &'static Mutex<Option<Arc<Telemetry>>> {
    GLOBAL_TELEMETRY.get_or_init(|| Mutex::new(None))
}

pub fn set_global_census_wake(hook: Arc<dyn CensusWake>) {
    let mut slot = global_wake().lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(hook);
}

pub fn set_global_census_telemetry(telemetry: Arc<Telemetry>) {
    let mut slot = global_telemetry().lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(telemetry);
}

/// Reset in-memory cache and global hooks (primarily for test isolation).
pub fn clear_title_cache() {
    title_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    *global_wake().lock().unwrap_or_else(|e| e.into_inner()) = None;
    *global_telemetry().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Append-only governor over `TITLE.md` documents (SPEC §10; ADR-0039).
///
/// Detects non-append mutations (edited/deleted prior entries) via prefix matching
/// and content digests on read. Fires sled trace + Mayor wake on tamper.
pub struct TitleWriter {
    tree: HamletTree,
    telemetry: Option<Arc<Telemetry>>,
    wake: Option<Arc<dyn CensusWake>>,
}

impl TitleWriter {
    #[must_use]
    pub fn new(tree: HamletTree) -> Self {
        Self {
            tree,
            telemetry: None,
            wake: None,
        }
    }

    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    #[must_use]
    pub fn with_wake(mut self, wake: Arc<dyn CensusWake>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Read and verify `TITLE.md` for `uid`. Returns `(doc, tampered)`.
    pub fn read_and_verify(&self, uid: u32) -> Result<(TitleDoc, bool), CensusError> {
        let path = if uid == MAYOR_UID {
            self.tree.manor_title()
        } else {
            self.tree.parcel_title(uid)
        };

        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => return Err(CensusError::NotRegularFile { path }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CensusError::Io {
                    context: format!("TITLE.md not found at {}", path.display()),
                    source: e,
                });
            }
            Err(source) => {
                return Err(CensusError::Io {
                    context: format!("stat {}", path.display()),
                    source,
                });
            }
        }

        let content = fs::read_to_string(&path).map_err(|source| CensusError::Io {
            context: format!("read {}", path.display()),
            source,
        })?;

        let doc = match TitleDoc::parse(&content) {
            Ok(d) => d,
            Err(e) => {
                let detail = format!("parse error: {e}");
                self.notify_tamper(uid, &detail);
                return Err(CensusError::Tamper { uid, detail });
            }
        };

        let mut tampered = false;
        let key = (self.tree.root().to_path_buf(), uid);
        {
            let mut cache = title_cache().lock().unwrap_or_else(|e| e.into_inner());
            if let Some((prev_entries, _prev_digest)) = cache.get(&key) {
                if let Err(e) = doc.verify_append_only(prev_entries) {
                    tampered = true;
                    let detail = format!("non-append mutation: {e}");
                    self.notify_tamper(uid, &detail);
                }
            }
            cache.insert(key, (doc.entries.clone(), doc.content_digest()));
        }

        Ok((doc, tampered))
    }

    /// Append one stamped entry to `TITLE.md` for `uid`.
    pub fn append(&self, uid: u32, entry: TitleEntry) -> Result<(), CensusError> {
        let path = if uid == MAYOR_UID {
            self.tree.manor_title()
        } else {
            self.tree.parcel_title(uid)
        };

        if path.exists() {
            let _ = self.read_and_verify(uid);
        }

        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => return Err(CensusError::NotRegularFile { path }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CensusError::Io {
                    context: format!("TITLE.md does not exist at {}", path.display()),
                    source: e,
                });
            }
            Err(source) => {
                return Err(CensusError::Io {
                    context: format!("stat {}", path.display()),
                    source,
                });
            }
        }

        let line = entry.render_line();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|source| CensusError::Io {
                context: format!("open append {}", path.display()),
                source,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|source| CensusError::Io {
                context: format!("append to {}", path.display()),
                source,
            })?;

        let key = (self.tree.root().to_path_buf(), uid);
        {
            let mut cache = title_cache().lock().unwrap_or_else(|e| e.into_inner());
            let (entries, digest) = cache
                .entry(key)
                .or_insert_with(|| (Vec::new(), String::new()));
            entries.push(entry);
            *digest = TitleDoc {
                header: format!(
                    "# TITLE — {}",
                    if uid == MAYOR_UID {
                        "manor".to_string()
                    } else {
                        format!("parcel{uid}")
                    }
                ),
                entries: entries.clone(),
            }
            .content_digest();
        }

        Ok(())
    }

    fn notify_tamper(&self, uid: u32, detail: &str) {
        let identity = if uid == MAYOR_UID {
            sanitize_name("manor").unwrap()
        } else {
            Charter::load(&self.tree.charter())
                .ok()
                .and_then(|c| c.slots.get(&uid).map(|s| s.identity.clone()))
                .unwrap_or_else(|| sanitize_name(&format!("parcel{uid}")).unwrap())
        };

        // 1. Sled trace
        let telem = self.telemetry.clone().or_else(|| {
            global_telemetry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if let Some(t) = telem {
            let at_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let _ = t.trace_event(
                AgentKey { commission: 0, uid },
                TraceRecord {
                    at_unix_ms,
                    event: "census-title-tamper".to_string(),
                    detail: detail.to_string(),
                },
            );
        }

        // 2. Mayor wake
        let wake = self.wake.clone().or_else(|| {
            global_wake()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if let Some(w) = wake {
            w.title_tampered(uid, &identity, detail);
        }
    }
}

/// Convenience helper to stamp one entry into `parcel<uid>/TITLE.md` or `manor/TITLE.md`.
pub fn stamp_parcel_title(
    tree: &HamletTree,
    uid: u32,
    entry: TitleEntry,
) -> Result<(), CensusError> {
    let writer = TitleWriter::new(tree.clone());
    writer.append(uid, entry)
}

/// Convenience helper to read and verify `TITLE.md` for `uid`.
pub fn read_parcel_title(tree: &HamletTree, uid: u32) -> Result<TitleDoc, CensusError> {
    let writer = TitleWriter::new(tree.clone());
    let (doc, _) = writer.read_and_verify(uid)?;
    Ok(doc)
}

/// Read census state across all parcels and manor (SPEC §10).
pub fn list_census(
    tree: &HamletTree,
    telemetry: Option<&Telemetry>,
) -> Result<Vec<CensusParcelView>, CensusError> {
    let charter = Charter::load(&tree.charter())?;
    let mut views = Vec::new();

    // 1. Manor (UID 1000)
    let manor_dir = tree.manor();
    if manor_dir.is_dir() {
        let manor_title_path = tree.manor_title();
        let (last_event, last_event_ts, title_digest, session_count) = if manor_title_path.is_file()
        {
            match TitleDoc::parse(&fs::read_to_string(&manor_title_path).unwrap_or_default()) {
                Ok(doc) => {
                    let last = doc.entries.last();
                    let le = last.map(|e| e.kind().to_string());
                    let lts = last.map(|e| e.timestamp().to_string());
                    let digest = doc.content_digest();
                    let sc = doc
                        .entries
                        .iter()
                        .filter(|e| e.kind() == TitleEntryKind::SessionIncarnation)
                        .count() as u32;
                    (le, lts, digest, sc.max(1))
                }
                Err(_) => (None, None, String::new(), 1),
            }
        } else {
            (None, None, String::new(), 1)
        };

        views.push(CensusParcelView {
            uid: MAYOR_UID,
            identity: sanitize_name("manor")?,
            commission: None,
            commission_slug: None,
            state: "exempt".to_string(),
            stasis: false,
            session_count,
            counters: CensusCounters::default(),
            last_event,
            last_event_ts,
            title_digest,
        });
    }

    // 2. Allocated slots from charter
    for (&uid, slot) in &charter.slots {
        if uid < FIRST_AGENT_UID {
            continue;
        }

        let lease = charter.parcels.get(&uid);
        let stasis = lease.is_some_and(|p| p.stasis);

        let state = if stasis {
            "stasis".to_string()
        } else if let Some(session) = charter.sessions.get(&uid) {
            session.status.to_string()
        } else {
            "idle".to_string()
        };

        let commission_slug = slot
            .commission
            .and_then(|c| charter.commissions.get(&c).map(|rec| rec.slug.clone()));

        let counters = if let (Some(telem), Some(comm)) = (telemetry, slot.commission) {
            if let Ok(totals) = telem.agent_totals(AgentKey {
                commission: comm,
                uid,
            }) {
                CensusCounters {
                    input_tokens: totals.input_tokens,
                    output_tokens: totals.output_tokens,
                    cost_micro_usd: totals.cost_micro_usd,
                    traces: totals.traces,
                }
            } else {
                CensusCounters::default()
            }
        } else {
            CensusCounters::default()
        };

        let title_path = if tree.parcel_title(uid).is_file() {
            tree.parcel_title(uid)
        } else {
            tree.parcel_archive(uid).join("TITLE.md")
        };
        let (last_event, last_event_ts, title_digest, session_count) = if title_path.is_file() {
            match TitleDoc::parse(&fs::read_to_string(&title_path).unwrap_or_default()) {
                Ok(doc) => {
                    let last = doc.entries.last();
                    let le = last.map(|e| e.kind().to_string());
                    let lts = last.map(|e| e.timestamp().to_string());
                    let digest = doc.content_digest();
                    let sc = doc
                        .entries
                        .iter()
                        .filter(|e| e.kind() == TitleEntryKind::SessionIncarnation)
                        .count() as u32;
                    (le, lts, digest, sc)
                }
                Err(_) => (None, None, String::new(), 0),
            }
        } else {
            (None, None, String::new(), 0)
        };

        views.push(CensusParcelView {
            uid,
            identity: slot.identity.clone(),
            commission: slot.commission,
            commission_slug,
            state,
            stasis,
            session_count,
            counters,
            last_event,
            last_event_ts,
            title_digest,
        });
    }

    views.sort_by_key(|v| v.uid);
    Ok(views)
}

/// Render a slice of [`CensusParcelView`] as a deterministic ASCII table
/// for `hamlet agent list` (SPEC §10, §12.1).
#[must_use]
pub fn render_agent_list(items: &[CensusParcelView]) -> String {
    let headers = [
        "UID",
        "IDENTITY",
        "COMMISSION",
        "STATE",
        "SESSIONS",
        "TRACES",
        "LAST_EVENT",
    ];
    if items.is_empty() {
        return format!(
            "{}  {}  {}  {}  {}  {}  {}\n(no agents)\n",
            headers[0], headers[1], headers[2], headers[3], headers[4], headers[5], headers[6]
        );
    }

    let rows: Vec<[String; 7]> = items
        .iter()
        .map(|item| {
            let comm = item.commission_slug.as_ref().map_or_else(
                || item.commission.map_or("-".to_string(), |c| c.to_string()),
                |s| s.to_string(),
            );
            let state = if item.stasis {
                "stasis".to_string()
            } else {
                item.state.clone()
            };
            let last_ev = item.last_event.as_deref().unwrap_or("-").to_string();

            [
                item.uid.to_string(),
                item.identity.to_string(),
                comm,
                state,
                item.session_count.to_string(),
                item.counters.traces.to_string(),
                last_ev,
            ]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
        headers[5].len(),
        headers[6].len(),
    ];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        headers[6],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
        w6 = widths[6],
    ));

    for row in rows {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            row[6],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
            w6 = widths[6],
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Part 6: Stasis and Hibernation Routine (SPEC §10; ADR-0039, ADR-0040)
// ---------------------------------------------------------------------------

/// Maximum home backup size guard: 256 MiB (SPEC §10).
pub const MAX_HOME_BACKUP_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Typed errors for hibernation and stasis operations (SPEC §10; ADR-0039, ADR-0040).
#[derive(Debug)]
pub enum StasisError {
    /// Slot does not exist or UID is invalid.
    UnknownSlot(u32),
    /// Slot identity is exempt from hibernation (e.g. Manor UID 1000).
    Exempt(u32),
    /// Slot is already in stasis (idempotent).
    AlreadyHibernated(u32),
    /// Session graceful stop failed.
    StopSession { uid: u32, source: String },
    /// Container runtime error (stop / remove / state).
    Runtime(String),
    /// Home backup exceeded maximum size guard.
    BackupSizeExceeded { total_bytes: u64, max_bytes: u64 },
    /// Home backup manifest creation or validation failure.
    Manifest(ManifestError),
    /// Missing PROPRIETOR.md in parcel.
    MissingProprietor(u32),
    /// PatronDoc validation or parsing failure.
    Patron(PatronError),
    /// Compute was not completely freed (container or session remained alive).
    ComputeNotFreed(String),
    /// Charter load/save failure.
    Charter(CharterError),
    /// Census TITLE.md stamping failure.
    Title(CensusError),
    /// Filesystem I/O error.
    Io {
        context: String,
        source: std::io::Error,
    },
    /// Post-teardown failure: the container was already destroyed before this error occurred.
    /// The parcel was marked crashed-hibernating with a full TITLE record.
    PostTeardown {
        cause: Box<StasisError>,
        detail: String,
    },
    /// Patron record not found.
    NoSuchPatron(SafeName),
    /// Patron document or backup manifest corruption.
    CorruptPatron(SafeName, String),
    /// Tamper detection or integrity mismatch in manifest.
    ManifestVerification(String),
    /// Unknown commission ID.
    UnknownCommission(u64),
    /// Commission is closed and cannot accept new agents.
    CommissionClosed(u64),
    /// No patron in tavern/patrons/ matches the requested skill configuration.
    NoMatchingPatron {
        register: SafeName,
        blueprint: SafeName,
    },
    /// Slot allocation failure.
    Slot(SlotError),
}

impl fmt::Display for StasisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSlot(uid) => write!(f, "stasis: unknown slot uid {uid}"),
            Self::Exempt(uid) => write!(f, "stasis: uid {uid} is exempt from hibernation"),
            Self::AlreadyHibernated(uid) => {
                write!(f, "stasis: slot uid {uid} is already hibernated")
            }
            Self::StopSession { uid, source } => {
                write!(f, "stasis: stop session for uid {uid} failed: {source}")
            }
            Self::Runtime(msg) => write!(f, "stasis: runtime error: {msg}"),
            Self::BackupSizeExceeded {
                total_bytes,
                max_bytes,
            } => {
                write!(
                    f,
                    "stasis: home backup size {total_bytes} bytes exceeds limit {max_bytes}"
                )
            }
            Self::Manifest(e) => write!(f, "stasis: manifest error: {e}"),
            Self::MissingProprietor(uid) => {
                write!(f, "stasis: missing PROPRIETOR.md for parcel {uid}")
            }
            Self::Patron(e) => write!(f, "stasis: patron error: {e}"),
            Self::ComputeNotFreed(msg) => write!(f, "stasis: compute not freed: {msg}"),
            Self::Charter(e) => write!(f, "stasis: charter error: {e}"),
            Self::Title(e) => write!(f, "stasis: title stamping error: {e}"),
            Self::Io { context, source } => write!(f, "stasis: {context}: {source}"),
            Self::PostTeardown { cause, detail } => {
                write!(f, "stasis: post-teardown failure ({detail}): {cause}")
            }
            Self::NoSuchPatron(name) => write!(f, "stasis: no such patron '{name}'"),
            Self::CorruptPatron(name, reason) => {
                write!(f, "stasis: corrupt patron '{name}': {reason}")
            }
            Self::ManifestVerification(reason) => {
                write!(f, "stasis: manifest verification failed: {reason}")
            }
            Self::UnknownCommission(id) => write!(f, "stasis: unknown commission {id}"),
            Self::CommissionClosed(id) => write!(f, "stasis: commission {id} is closed"),
            Self::NoMatchingPatron {
                register,
                blueprint,
            } => {
                write!(
                    f,
                    "stasis: no patron matches archetype '{register}' and blueprint '{blueprint}'"
                )
            }
            Self::Slot(e) => write!(f, "stasis: slot allocation error: {e}"),
        }
    }
}

impl std::error::Error for StasisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(e) => Some(e),
            Self::Patron(e) => Some(e),
            Self::Charter(e) => Some(e),
            Self::Title(e) => Some(e),
            Self::Io { source, .. } => Some(source),
            Self::PostTeardown { cause, .. } => Some(cause.as_ref()),
            Self::Slot(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CharterError> for StasisError {
    fn from(e: CharterError) -> Self {
        Self::Charter(e)
    }
}

impl From<SlotError> for StasisError {
    fn from(e: SlotError) -> Self {
        Self::Slot(e)
    }
}

impl From<PodmanError> for StasisError {
    fn from(e: PodmanError) -> Self {
        Self::Runtime(e.to_string())
    }
}

impl From<BootstrapError> for StasisError {
    fn from(e: BootstrapError) -> Self {
        Self::Runtime(format!("bootstrap: {e}"))
    }
}

impl From<HostPermError> for StasisError {
    fn from(e: HostPermError) -> Self {
        Self::Io {
            context: "host permissions".to_string(),
            source: std::io::Error::other(e.to_string()),
        }
    }
}

impl From<ManifestError> for StasisError {
    fn from(e: ManifestError) -> Self {
        Self::Manifest(e)
    }
}

impl From<PatronError> for StasisError {
    fn from(e: PatronError) -> Self {
        Self::Patron(e)
    }
}

impl From<CensusError> for StasisError {
    fn from(e: CensusError) -> Self {
        Self::Title(e)
    }
}

/// An agent awakened from a hibernated patron (SPEC §10; ADR-0039).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwakenedAgent {
    /// Fresh slot UID assigned to the awakened agent.
    pub uid: u32,
    /// Identity of the awakened agent (bound to patron name).
    pub identity: SafeName,
    /// Commission ID if awakened for a commission.
    pub commission: Option<u64>,
    /// Patron name from which the agent was awakened.
    pub patron: SafeName,
    /// Parcel lease recorded in charter.
    pub lease: ParcelLease,
    /// Restored patron profile document.
    pub patron_doc: PatronDoc,
}

/// Production implementation of the [`Stasis`] seam (SPEC §10; ADR-0039, ADR-0040).
pub struct ProductionStasis {
    tree: HamletTree,
    runtime: Arc<dyn Runtime>,
    supervisor: Option<Arc<Supervisor>>,
    telemetry: Option<Arc<Telemetry>>,
    secrets: Option<Secrets>,
    max_home_bytes: u64,
}

impl ProductionStasis {
    /// Create a new stasis executor for a village tree.
    #[must_use]
    pub fn new(tree: HamletTree, runtime: Arc<dyn Runtime>) -> Self {
        Self {
            tree,
            runtime,
            supervisor: None,
            telemetry: None,
            secrets: None,
            max_home_bytes: MAX_HOME_BACKUP_TOTAL_BYTES,
        }
    }

    /// Attach session supervisor for graceful drains before teardown.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Arc<Supervisor>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// Attach sled telemetry for event tracing.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Attach pre-hydrated secrets for session startup.
    #[must_use]
    pub fn with_secrets(mut self, secrets: Secrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Override max home backup size guard (primarily for testing size limits).
    #[must_use]
    pub fn with_max_home_bytes(mut self, max_bytes: u64) -> Self {
        self.max_home_bytes = max_bytes;
        self
    }

    /// Village tree accessor.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Runtime seam accessor.
    #[must_use]
    pub fn runtime(&self) -> &dyn Runtime {
        self.runtime.as_ref()
    }

    /// Execute the complete 5-step hibernation routine asynchronously.
    pub async fn hibernate(&self, uid: u32, trigger: &str) -> Result<PatronDoc, StasisError> {
        self.hibernate_internal(uid, trigger).await
    }

    /// Synchronous wrapper around [`Self::hibernate`].
    pub fn hibernate_sync(&self, uid: u32, trigger: &str) -> Result<PatronDoc, StasisError> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                tokio::task::block_in_place(|| handle.block_on(self.hibernate(uid, trigger)))
            } else {
                std::thread::scope(|s| {
                    s.spawn(|| {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| StasisError::Runtime(format!("tokio runtime: {e}")))?;
                        rt.block_on(self.hibernate(uid, trigger))
                    })
                    .join()
                    .unwrap_or_else(|_| {
                        Err(StasisError::Runtime("worker thread panicked".to_string()))
                    })
                })
            }
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| StasisError::Runtime(format!("tokio runtime: {e}")))?;
            rt.block_on(self.hibernate(uid, trigger))
        }
    }

    /// Find a patron in `tavern/patrons/` matching skill configuration (SPEC §10; ADR-0039).
    pub fn find_matching_patron(
        &self,
        register: &SafeName,
        blueprint: &SafeName,
        required_tags: &[SafeName],
    ) -> Result<Option<PatronDoc>, StasisError> {
        let patrons_dir = self.tree.tavern_patrons();
        if !patrons_dir.is_dir() {
            return Ok(None);
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&patrons_dir).map_err(|e| StasisError::Io {
            context: format!("read dir {}", patrons_dir.display()),
            source: e,
        })? {
            let entry = entry.map_err(|e| StasisError::Io {
                context: format!("read dir entry in {}", patrons_dir.display()),
                source: e,
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("PROPRIETOR") && entry.path().is_dir() {
                entries.push((name, entry.path()));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (_, path) in entries {
            let prop_path = path.join("PROPRIETOR.md");
            if prop_path.is_file() {
                if let Ok(content) = fs::read_to_string(&prop_path) {
                    if let Ok(doc) = PatronDoc::parse(&content) {
                        if doc.matches_skill_config(register, blueprint, required_tags) {
                            return Ok(Some(doc));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Read and parse a patron by name from `tavern/patrons/PROPRIETOR<name>/`.
    pub fn find_patron_by_name(&self, patron: &SafeName) -> Result<PatronDoc, StasisError> {
        let patron_dir = self.tree.patron(patron);
        if !patron_dir.is_dir() {
            return Err(StasisError::NoSuchPatron(patron.clone()));
        }
        let patron_md = self.tree.patron_md(patron);
        if !patron_md.is_file() {
            return Err(StasisError::CorruptPatron(
                patron.clone(),
                "missing PROPRIETOR.md".to_string(),
            ));
        }
        let content = fs::read_to_string(&patron_md).map_err(|e| StasisError::Io {
            context: format!("read {}", patron_md.display()),
            source: e,
        })?;
        PatronDoc::parse(&content).map_err(StasisError::Patron)
    }

    /// Awaken a hibernated patron matching the given skill configuration (SPEC §10).
    pub async fn awaken_by_skill(
        &self,
        register: &SafeName,
        blueprint: &SafeName,
        required_tags: &[SafeName],
        commission: Option<u64>,
    ) -> Result<AwakenedAgent, StasisError> {
        let matched = self
            .find_matching_patron(register, blueprint, required_tags)?
            .ok_or_else(|| StasisError::NoMatchingPatron {
                register: register.clone(),
                blueprint: blueprint.clone(),
            })?;
        self.awaken(&matched.frontmatter.patron, commission).await
    }

    /// Synchronous wrapper around [`Self::awaken`].
    pub fn awaken_sync(
        &self,
        patron_ref: &SafeName,
        commission: Option<u64>,
    ) -> Result<AwakenedAgent, StasisError> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                tokio::task::block_in_place(|| handle.block_on(self.awaken(patron_ref, commission)))
            } else {
                std::thread::scope(|s| {
                    s.spawn(|| {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| StasisError::Runtime(format!("tokio runtime: {e}")))?;
                        rt.block_on(self.awaken(patron_ref, commission))
                    })
                    .join()
                    .unwrap_or_else(|_| {
                        Err(StasisError::Runtime("worker thread panicked".to_string()))
                    })
                })
            }
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| StasisError::Runtime(format!("tokio runtime: {e}")))?;
            rt.block_on(self.awaken(patron_ref, commission))
        }
    }

    /// Awaken a hibernated patron into a fresh UID slot and active parcel lease (SPEC §10; ADR-0039).
    pub async fn awaken(
        &self,
        patron_ref: &SafeName,
        commission: Option<u64>,
    ) -> Result<AwakenedAgent, StasisError> {
        // Step 1: Validate patron record existence and documents
        let patron_dir = self.tree.patron(patron_ref);
        if !patron_dir.is_dir() {
            return Err(StasisError::NoSuchPatron(patron_ref.clone()));
        }

        let patron_md = self.tree.patron_md(patron_ref);
        if !patron_md.is_file() {
            return Err(StasisError::CorruptPatron(
                patron_ref.clone(),
                "missing PROPRIETOR.md".to_string(),
            ));
        }
        let prop_content = fs::read_to_string(&patron_md).map_err(|e| StasisError::Io {
            context: format!("read {}", patron_md.display()),
            source: e,
        })?;
        let patron_doc = PatronDoc::parse(&prop_content).map_err(StasisError::Patron)?;

        let manifest_path = self.tree.patron_manifest(patron_ref);
        if !manifest_path.is_file() {
            return Err(StasisError::CorruptPatron(
                patron_ref.clone(),
                "missing manifest.json".to_string(),
            ));
        }
        let manifest_content = fs::read_to_string(&manifest_path).map_err(|e| StasisError::Io {
            context: format!("read {}", manifest_path.display()),
            source: e,
        })?;
        let manifest = BackupManifest::parse(&manifest_content).map_err(StasisError::Manifest)?;

        if manifest.patron != *patron_ref {
            return Err(StasisError::ManifestVerification(format!(
                "manifest patron '{}' does not match requested patron '{}'",
                manifest.patron, patron_ref
            )));
        }

        // Step 2: Validate backup directory integrity and tamper detection against manifest
        let bak_dir = self.tree.patron_home_backup(patron_ref);
        if !bak_dir.is_dir() {
            return Err(StasisError::CorruptPatron(
                patron_ref.clone(),
                "missing home.uid.bak directory".to_string(),
            ));
        }
        verify_backup_integrity(&bak_dir, &manifest)?;

        // Step 3: Commission check & Charter slot allocation
        let mut charter = Charter::load(&self.tree.charter())?;
        if let Some(cid) = commission {
            let comm = charter
                .commissions
                .get(&cid)
                .ok_or(StasisError::UnknownCommission(cid))?;
            if !comm.state().is_live() {
                return Err(StasisError::CommissionClosed(cid));
            }
        }

        let prior_commission = patron_doc.frontmatter.commission;
        let new_uid = charter.allocate_fresh_slot(patron_ref.clone(), commission)?;

        if let Some(cid) = commission {
            if let Some(comm) = charter.commissions.get_mut(&cid) {
                if comm.state().is_live() {
                    let _ = comm.assign_uid(new_uid);
                }
            }
        }

        let lease = ParcelLease {
            identity: patron_ref.clone(),
            stasis: false,
        };
        charter.parcels.insert(new_uid, lease.clone());
        charter
            .agents
            .insert(patron_ref.clone(), AgentRef { slot: new_uid });
        charter.validate()?;
        charter.save_atomic(&self.tree.charter())?;

        // Step 4: Provision parcel directory and restore home from backup
        let parcel_dir = self.tree.parcel(new_uid);
        fs::create_dir_all(&parcel_dir).map_err(|e| StasisError::Io {
            context: format!("create parcel dir {}", parcel_dir.display()),
            source: e,
        })?;
        set_mode(&parcel_dir, MODE_PARCEL_DIR)?;

        let parcel_home = self.tree.parcel_home(new_uid);
        fs::create_dir_all(&parcel_home).map_err(|e| StasisError::Io {
            context: format!("create parcel home {}", parcel_home.display()),
            source: e,
        })?;
        set_mode(&parcel_home, MODE_PARCEL_DIR)?;

        restore_dir_recursive(&bak_dir, &parcel_home)?;

        if let Some(cid) = commission {
            let git_dir = self.tree.commission_git(cid);
            let _ = fs::create_dir_all(&git_dir);
        }
        let recipient = sanitize_name(&new_uid.to_string()).unwrap_or_else(|_| patron_ref.clone());
        let spool = self.tree.courier_mail_recipient(&recipient);
        let _ = fs::create_dir_all(&spool);

        let perms = HostPermissions::new(self.runtime.as_ref());
        let _ = perms.apply_parcel_home(&self.tree, new_uid).await;
        let _ = perms.apply_mail_spool(&self.tree, new_uid).await;
        if let Some(cid) = commission {
            let _ = perms.apply_shared(&self.tree, cid).await;
        }

        // Step 5: Seed PROPRIETOR.md with updated lineage
        let mut awakened_patron_doc = patron_doc.clone();
        awakened_patron_doc.frontmatter.source_uid = new_uid;
        awakened_patron_doc.frontmatter.commission = commission;
        fs::write(
            self.tree.parcel_proprietor(new_uid),
            awakened_patron_doc.render(),
        )
        .map_err(|e| StasisError::Io {
            context: format!("write {}", self.tree.parcel_proprietor(new_uid).display()),
            source: e,
        })?;

        // Step 6: Seed DEED.md from blueprint
        let blueprint_name = &patron_doc.frontmatter.blueprint;
        let version = patron_doc.frontmatter.blueprint_version;
        let bp_ver_path = self.tree.guild_blueprint_version(blueprint_name, version);
        let bp_path = self.tree.guild_blueprint(blueprint_name);
        let blueprint = if bp_ver_path.is_file() {
            let s = fs::read_to_string(&bp_ver_path).unwrap_or_default();
            Blueprint::from_toml(&s)
                .unwrap_or_else(|_| Blueprint::default_village_alpine().unwrap())
        } else if bp_path.is_file() {
            let s = fs::read_to_string(&bp_path).unwrap_or_default();
            Blueprint::from_toml(&s)
                .unwrap_or_else(|_| Blueprint::default_village_alpine().unwrap())
        } else {
            Blueprint::default_village_alpine().unwrap()
        };

        fs::write(
            self.tree.parcel_deed(new_uid),
            deed_stub(patron_ref, new_uid, commission.unwrap_or(0), &blueprint),
        )
        .map_err(|e| StasisError::Io {
            context: format!("write {}", self.tree.parcel_deed(new_uid).display()),
            source: e,
        })?;

        // Step 7: Opening TITLE.md entry (lineage: awakened-from patron + prior commission)
        let awaken_entry = TitleEntry::Awaken {
            timestamp: now_iso8601(),
            patron: patron_ref.clone(),
            from_commission: prior_commission,
        };
        let mut title_content = format!("# TITLE — parcel{new_uid}\n\n");
        title_content.push_str(&awaken_entry.render_line());
        write_stub(&self.tree.parcel_title(new_uid), &title_content).map_err(|e| {
            StasisError::Io {
                context: format!("write {}", self.tree.parcel_title(new_uid).display()),
                source: std::io::Error::other(e.to_string()),
            }
        })?;
        {
            let key = (self.tree.root().to_path_buf(), new_uid);
            let mut cache = title_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                key,
                (
                    vec![awaken_entry],
                    TitleDoc {
                        header: format!("# TITLE — parcel{new_uid}"),
                        entries: vec![TitleEntry::Awaken {
                            timestamp: now_iso8601(),
                            patron: patron_ref.clone(),
                            from_commission: prior_commission,
                        }],
                    }
                    .content_digest(),
                ),
            );
        }

        // Step 8: Container creation with derived image (fast path: NO clean bootstrap)
        let bootstrapper = Bootstrapper::new(&self.tree, self.runtime.as_ref())?;
        let image = bootstrapper.ensure_image(&blueprint).await?;
        let config = if self.tree.config().is_file() {
            fs::read_to_string(self.tree.config())
                .ok()
                .and_then(|s| Config::parse(&s).ok())
                .unwrap_or_default()
        } else {
            Config::default()
        };
        let spec = RunSpec::for_agent(
            patron_ref,
            new_uid,
            commission.unwrap_or(0),
            &blueprint,
            &config,
        )?
        .with_image(image.tag.clone());
        let _ = self.runtime.run_detached(&spec, &self.tree).await?;
        let _ =
            crate::ledger::seed_agent_git_home(self.runtime.as_ref(), patron_ref, new_uid).await;

        // Step 9: Session start with patron intelligence profile as brief overlay
        if let Some(sup) = &self.supervisor {
            let register_ref = &patron_doc.frontmatter.register;
            let mut brief =
                fs::read_to_string(self.tree.register_brief(register_ref)).unwrap_or_default();
            if !patron_doc.body.trim().is_empty() {
                if !brief.is_empty() && !brief.ends_with('\n') {
                    brief.push('\n');
                }
                brief.push_str("\n## Patron Intelligence Profile\n\n");
                brief.push_str(&patron_doc.body);
                if !brief.ends_with('\n') {
                    brief.push('\n');
                }
            }
            let parcel_ref = ParcelRef::new(new_uid, patron_ref.clone(), commission.unwrap_or(0));
            let secrets = self
                .secrets
                .clone()
                .unwrap_or_else(|| crate::dispatch::load_secrets(&self.tree).unwrap_or_default());
            let _ = sup.start_session(&parcel_ref, &brief, &secrets).await;
        }

        // Step 10: Sled telemetry trace
        let telem = self.telemetry.clone().or_else(|| {
            global_telemetry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if let Some(tel) = telem {
            let record = TraceRecord {
                at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                event: "agent-awakened".to_string(),
                detail: scrub_detail(&format!(
                    "patron={patron_ref} uid={new_uid} prior_commission={prior_commission:?}"
                )),
            };
            let key = AgentKey {
                commission: commission.unwrap_or(0),
                uid: new_uid,
            };
            let _ = tel.trace_event(key, record);
        }

        // Step 11: Remove patron record from tavern/patrons/ once awakening succeeds
        let _ = fs::remove_dir_all(&patron_dir);

        Ok(AwakenedAgent {
            uid: new_uid,
            identity: patron_ref.clone(),
            commission,
            patron: patron_ref.clone(),
            lease,
            patron_doc: awakened_patron_doc,
        })
    }

    async fn hibernate_internal(&self, uid: u32, trigger: &str) -> Result<PatronDoc, StasisError> {
        // Step 0: Pre-flight checks (before side effects)
        if uid == MAYOR_UID {
            return Err(StasisError::Exempt(uid));
        }
        if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(StasisError::UnknownSlot(uid));
        }

        let (identity, commission, already_hibernated) = {
            let charter = Charter::load(&self.tree.charter())?;
            let slot = charter
                .slots
                .get(&uid)
                .ok_or(StasisError::UnknownSlot(uid))?;
            let is_hib = slot.status == SlotStatus::Hibernated
                || charter.parcels.get(&uid).is_some_and(|p| p.stasis);
            (slot.identity.clone(), slot.commission, is_hib)
        };

        if already_hibernated {
            let patron_md = self.tree.patron_md(&identity);
            if patron_md.is_file() {
                let content = fs::read_to_string(&patron_md).map_err(|e| StasisError::Io {
                    context: format!("read {}", patron_md.display()),
                    source: e,
                })?;
                return PatronDoc::parse(&content).map_err(StasisError::Patron);
            }
            return Err(StasisError::AlreadyHibernated(uid));
        }

        let parcel_dir = self.tree.parcel(uid);
        if !parcel_dir.is_dir() {
            return Err(StasisError::Io {
                context: format!("parcel dir {}", parcel_dir.display()),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "parcel dir missing"),
            });
        }

        // Step 1: Drain delivery queue -> Graceful stop -> Container teardown
        if let Some(sup) = &self.supervisor {
            let rec = sup
                .session_record(uid)
                .map_err(|e| StasisError::StopSession {
                    uid,
                    source: e.to_string(),
                })?;
            if let Some(rec) = rec {
                if rec.status.is_alive() && sup.is_live(uid) {
                    match sup.stop_session(uid).await {
                        Ok(_) | Err(crate::supervise::SuperviseError::NotLive(_)) => {}
                        Err(e) => {
                            return Err(StasisError::StopSession {
                                uid,
                                source: e.to_string(),
                            });
                        }
                    }
                }
            }
        }

        let state = self
            .runtime
            .state(identity.as_str())
            .await
            .unwrap_or(ContainerState::Unknown);
        if state == ContainerState::Running {
            let _ = self
                .runtime
                .stop(identity.as_str(), Duration::from_secs(5))
                .await;
        }

        // Container teardown
        self.runtime
            .remove(identity.as_str(), true)
            .await
            .map_err(|e| StasisError::Runtime(format!("container remove {identity}: {e}")))?;

        // FROM THIS POINT: Container is destroyed. Failures invoke handle_post_teardown_failure.
        match self
            .hibernate_post_teardown(uid, &identity, commission, trigger)
            .await
        {
            Ok(doc) => Ok(doc),
            Err(e) => {
                self.handle_post_teardown_failure(uid, &identity, commission, &e)
                    .await;
                Err(StasisError::PostTeardown {
                    cause: Box::new(e),
                    detail: format!("hibernation failed for uid {uid} after container teardown"),
                })
            }
        }
    }

    async fn hibernate_post_teardown(
        &self,
        uid: u32,
        identity: &SafeName,
        commission: Option<u64>,
        trigger: &str,
    ) -> Result<PatronDoc, StasisError> {
        let patrons_dir = self.tree.tavern_patrons();
        fs::create_dir_all(&patrons_dir).map_err(|e| StasisError::Io {
            context: format!("create dir {}", patrons_dir.display()),
            source: e,
        })?;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp_dir = patrons_dir.join(format!(".tmp_PROPRIETOR{}_{}_{}", identity, uid, nanos));
        fs::create_dir_all(&tmp_dir).map_err(|e| StasisError::Io {
            context: format!("create tmp dir {}", tmp_dir.display()),
            source: e,
        })?;

        let tmp_home_bak = tmp_dir.join("home.uid.bak");
        fs::create_dir_all(&tmp_home_bak).map_err(|e| StasisError::Io {
            context: format!("create tmp home backup {}", tmp_home_bak.display()),
            source: e,
        })?;

        // Step 2: Copy parcel home/ -> home.uid.bak/ with size guard + manifest checksums
        let src_home = self.tree.parcel_home(uid);
        let mut total_bytes = 0u64;
        if src_home.is_dir() {
            copy_dir_recursive(
                &src_home,
                &tmp_home_bak,
                &mut total_bytes,
                self.max_home_bytes,
            )?;
        }

        // Build BackupManifest
        let mut manifest_entries = BTreeMap::new();
        for entry in fs::read_dir(&tmp_home_bak).map_err(|e| StasisError::Io {
            context: format!("read dir {}", tmp_home_bak.display()),
            source: e,
        })? {
            let entry = entry.map_err(|e| StasisError::Io {
                context: format!("read entry in {}", tmp_home_bak.display()),
                source: e,
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| StasisError::Io {
                context: format!("stat {}", path.display()),
                source: e,
            })?;

            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0o644;

            let (entry_type, digest, size_bytes) = if meta.is_dir() {
                let digest = compute_dir_digest(&path).map_err(|e| StasisError::Io {
                    context: format!("compute dir digest {}", path.display()),
                    source: e,
                })?;
                (ManifestEntryType::Directory, digest, meta.len())
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(|e| StasisError::Io {
                    context: format!("readlink {}", path.display()),
                    source: e,
                })?;
                let digest = content_digest(target.to_string_lossy().as_bytes());
                (ManifestEntryType::Symlink, digest, meta.len())
            } else {
                let bytes = fs::read(&path).map_err(|e| StasisError::Io {
                    context: format!("read file {}", path.display()),
                    source: e,
                })?;
                let digest = content_digest(&bytes);
                (ManifestEntryType::File, digest, meta.len())
            };

            manifest_entries.insert(
                name,
                ManifestEntry {
                    entry_type,
                    digest,
                    size_bytes,
                    mode,
                },
            );
        }

        let manifest = BackupManifest {
            manifest_version: MANIFEST_SCHEMA_VERSION,
            patron: identity.clone(),
            source_uid: uid,
            created_at: unix_now(),
            total_bytes,
            entries: manifest_entries,
        };

        // Verify all top-level entries against manifest
        for (name, entry) in &manifest.entries {
            manifest.verify_entry(name, entry.entry_type, entry.size_bytes, &entry.digest)?;
        }

        let manifest_json = manifest.render()?;
        fs::write(tmp_dir.join("manifest.json"), manifest_json).map_err(|e| StasisError::Io {
            context: format!("write {}", tmp_dir.join("manifest.json").display()),
            source: e,
        })?;

        // Step 3: Serialize the mutated agent profile
        let prop_path = self.tree.parcel_proprietor(uid);
        if !prop_path.is_file() {
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(StasisError::MissingProprietor(uid));
        }
        let prop_content = fs::read_to_string(&prop_path).map_err(|e| StasisError::Io {
            context: format!("read {}", prop_path.display()),
            source: e,
        })?;

        let patron_doc = if prop_content.trim_start().starts_with("---") {
            let doc = PatronDoc::parse(&prop_content).map_err(StasisError::Patron)?;
            if doc.frontmatter.source_uid != uid {
                let _ = fs::remove_dir_all(&tmp_dir);
                return Err(StasisError::Patron(PatronError::InvalidField {
                    field: "source_uid",
                    reason: format!(
                        "source_uid {} does not match slot {uid}",
                        doc.frontmatter.source_uid
                    ),
                }));
            }
            doc
        } else {
            if prop_content.contains('\r') {
                let _ = fs::remove_dir_all(&tmp_dir);
                return Err(StasisError::Patron(PatronError::BadLineEnding { line: 1 }));
            }
            if prop_content.len() > MAX_PROPRIETOR_BODY_BYTES {
                let _ = fs::remove_dir_all(&tmp_dir);
                return Err(StasisError::Patron(PatronError::BodyTooLarge {
                    len: prop_content.len(),
                }));
            }
            let (archetype, blueprint, blueprint_version) =
                read_parcel_blueprint_info(&self.tree, uid)
                    .unwrap_or_else(|| (identity.clone(), sanitize_name("default").unwrap(), 1));
            let frontmatter = PatronFrontmatter {
                patron: identity.clone(),
                register: archetype,
                blueprint,
                blueprint_version,
                source_uid: uid,
                commission,
                tags: BTreeSet::new(),
                created_at: unix_now(),
                stasis_version: PATRON_SCHEMA_VERSION,
            };
            PatronDoc {
                frontmatter,
                body: prop_content,
            }
        };

        fs::write(tmp_dir.join("PROPRIETOR.md"), patron_doc.render()).map_err(|e| {
            StasisError::Io {
                context: format!("write {}", tmp_dir.join("PROPRIETOR.md").display()),
                source: e,
            }
        })?;

        // Atomic commit of patron record
        let patron_dir = self.tree.patron(identity);
        if patron_dir.exists() {
            fs::remove_dir_all(&patron_dir).map_err(|e| StasisError::Io {
                context: format!("remove old patron dir {}", patron_dir.display()),
                source: e,
            })?;
        }
        fs::rename(&tmp_dir, &patron_dir).map_err(|e| StasisError::Io {
            context: format!("rename {} to {}", tmp_dir.display(), patron_dir.display()),
            source: e,
        })?;

        // Step 4: charter status -> hibernated; census TITLE entry; sled trace; compute freed
        let hibernate_entry = TitleEntry::Hibernate {
            timestamp: now_iso8601(),
            patron: identity.clone(),
            reason: scrub_detail(trigger),
        };
        stamp_parcel_title(&self.tree, uid, hibernate_entry)?;

        let mut charter = Charter::load(&self.tree.charter())?;
        if let Some(slot) = charter.slots.get_mut(&uid) {
            slot.status = SlotStatus::Hibernated;
        }
        if let Some(lease) = charter.parcels.get_mut(&uid) {
            lease.stasis = true;
        } else {
            charter.parcels.insert(
                uid,
                ParcelLease {
                    identity: identity.clone(),
                    stasis: true,
                },
            );
        }
        if let Some(session) = charter.sessions.get_mut(&uid) {
            session.status = SessionStatus::Stopped;
        }
        charter.save_atomic(&self.tree.charter())?;

        // Sled trace
        let telem = self.telemetry.clone().or_else(|| {
            global_telemetry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if let Some(tel) = telem {
            let record = TraceRecord {
                at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                event: "agent-hibernated".to_string(),
                detail: format!("patron={identity} trigger={trigger}"),
            };
            let key = AgentKey {
                commission: commission.unwrap_or(0),
                uid,
            };
            let _ = tel.trace_event(key, record);
        }

        // Assert compute freed
        let state = self
            .runtime
            .state(identity.as_str())
            .await
            .unwrap_or(ContainerState::Unknown);
        if state == ContainerState::Running {
            return Err(StasisError::ComputeNotFreed(format!(
                "container {identity} still running after teardown"
            )));
        }
        if let Some(sup) = &self.supervisor {
            if sup.is_live(uid) {
                return Err(StasisError::ComputeNotFreed(format!(
                    "session {uid} still marked live in supervisor"
                )));
            }
        }

        // Step 5: parcel dir disposition: archive metadata to keep/archives/ and release parcel dir
        let archive_dir = self.tree.parcel_archive(uid);
        fs::create_dir_all(&archive_dir).map_err(|e| StasisError::Io {
            context: format!("create archive dir {}", archive_dir.display()),
            source: e,
        })?;

        fs::copy(self.tree.parcel_title(uid), archive_dir.join("TITLE.md")).map_err(|e| {
            StasisError::Io {
                context: format!("archive TITLE.md for parcel {uid}"),
                source: e,
            }
        })?;
        if self.tree.parcel_deed(uid).is_file() {
            let _ = fs::copy(self.tree.parcel_deed(uid), archive_dir.join("DEED.md"));
        }
        if self.tree.parcel_proprietor(uid).is_file() {
            let _ = fs::copy(
                self.tree.parcel_proprietor(uid),
                archive_dir.join("PROPRIETOR.md"),
            );
        }

        let parcel_dir = self.tree.parcel(uid);
        if parcel_dir.exists() {
            fs::remove_dir_all(&parcel_dir).map_err(|e| StasisError::Io {
                context: format!("remove parcel dir {}", parcel_dir.display()),
                source: e,
            })?;
        }

        Ok(patron_doc)
    }

    async fn handle_post_teardown_failure(
        &self,
        uid: u32,
        identity: &SafeName,
        commission: Option<u64>,
        err: &StasisError,
    ) {
        // 1. Mark charter status as Crashed
        if let Ok(mut charter) = Charter::load(&self.tree.charter()) {
            if let Some(slot) = charter.slots.get_mut(&uid) {
                slot.status = SlotStatus::Crashed;
            }
            if let Some(session) = charter.sessions.get_mut(&uid) {
                session.status = SessionStatus::Crashed;
            }
            let _ = charter.save_atomic(&self.tree.charter());
        }

        // 2. Stamp crash entry in TITLE.md
        let crash_entry = TitleEntry::Crash {
            timestamp: now_iso8601(),
            session_id: format!("s-{uid}-hibernation"),
            exit_code: None,
            restarts: 0,
            reason: scrub_detail(&format!("hibernation failed after teardown: {err}")),
        };
        let _ = stamp_parcel_title(&self.tree, uid, crash_entry);

        // 3. Sled telemetry trace
        let telem = self.telemetry.clone().or_else(|| {
            global_telemetry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if let Some(tel) = telem {
            let record = TraceRecord {
                at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                event: "session-hibernation-failed".to_string(),
                detail: scrub_detail(&format!("patron={identity} error={err}")),
            };
            let key = AgentKey {
                commission: commission.unwrap_or(0),
                uid,
            };
            let _ = tel.trace_event(key, record);
        }
    }
}

impl Stasis for ProductionStasis {
    fn park<'a>(&'a self, uid: u32) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        <Self as Stasis>::hibernate(self, uid, "decree:hibernate_agent")
    }

    fn hibernate<'a>(
        &'a self,
        uid: u32,
        trigger: &'a str,
    ) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        Box::pin(async move {
            self.hibernate_internal(uid, trigger)
                .await
                .map(|_| ())
                .map_err(|e| match e {
                    StasisError::UnknownSlot(u) => DecreeRefusal::unknown_slot(u),
                    StasisError::Exempt(u) => {
                        DecreeRefusal::invalid_target(format!("uid {u} is exempt from hibernation"))
                    }
                    other => DecreeRefusal::execution_failed(other.to_string()),
                })
        })
    }
}

fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    total_bytes: &mut u64,
    max_bytes: u64,
) -> Result<(), StasisError> {
    if !dst.exists() {
        fs::create_dir_all(dst).map_err(|e| StasisError::Io {
            context: format!("create dir {}", dst.display()),
            source: e,
        })?;
    }
    for entry in fs::read_dir(src).map_err(|e| StasisError::Io {
        context: format!("read dir {}", src.display()),
        source: e,
    })? {
        let entry = entry.map_err(|e| StasisError::Io {
            context: format!("read dir entry in {}", src.display()),
            source: e,
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&src_path).map_err(|e| StasisError::Io {
            context: format!("stat {}", src_path.display()),
            source: e,
        })?;
        if meta.is_dir() {
            copy_dir_recursive(&src_path, &dst_path, total_bytes, max_bytes)?;
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(&src_path).map_err(|e| StasisError::Io {
                context: format!("readlink {}", src_path.display()),
                source: e,
            })?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dst_path).map_err(|e| StasisError::Io {
                context: format!("symlink {}", dst_path.display()),
                source: e,
            })?;
        } else {
            let len = meta.len();
            if *total_bytes + len > max_bytes {
                return Err(StasisError::BackupSizeExceeded {
                    total_bytes: *total_bytes + len,
                    max_bytes,
                });
            }
            *total_bytes += len;
            fs::copy(&src_path, &dst_path).map_err(|e| StasisError::Io {
                context: format!("copy {} to {}", src_path.display(), dst_path.display()),
                source: e,
            })?;
            #[cfg(unix)]
            {
                let perms = meta.permissions();
                let _ = fs::set_permissions(&dst_path, perms);
            }
        }
    }
    Ok(())
}

fn verify_backup_integrity(bak_dir: &Path, manifest: &BackupManifest) -> Result<(), StasisError> {
    for (name, entry) in &manifest.entries {
        let path = bak_dir.join(name);
        if !path.exists() && !path.is_symlink() {
            return Err(StasisError::ManifestVerification(format!(
                "manifest entry '{name}' missing on disk"
            )));
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| StasisError::Io {
            context: format!("stat {}", path.display()),
            source: e,
        })?;
        if meta.file_type().is_symlink() || entry.entry_type == ManifestEntryType::Symlink {
            return Err(StasisError::ManifestVerification(format!(
                "refusing symlink '{name}' during restore (ADR-0007, ADR-0039)"
            )));
        }
        if meta.is_dir() {
            if entry.entry_type != ManifestEntryType::Directory {
                return Err(StasisError::ManifestVerification(format!(
                    "entry '{name}' expected directory, found other"
                )));
            }
            let digest = compute_dir_digest(&path).map_err(|e| StasisError::Io {
                context: format!("compute dir digest {}", path.display()),
                source: e,
            })?;
            if digest != entry.digest {
                return Err(StasisError::ManifestVerification(format!(
                    "directory '{name}' digest mismatch: expected {}, got {digest}",
                    entry.digest
                )));
            }
        } else {
            if entry.entry_type != ManifestEntryType::File {
                return Err(StasisError::ManifestVerification(format!(
                    "entry '{name}' expected file, found other"
                )));
            }
            if meta.len() != entry.size_bytes {
                return Err(StasisError::ManifestVerification(format!(
                    "file '{name}' size mismatch: expected {} bytes, got {} bytes",
                    entry.size_bytes,
                    meta.len()
                )));
            }
            let bytes = fs::read(&path).map_err(|e| StasisError::Io {
                context: format!("read file {}", path.display()),
                source: e,
            })?;
            let digest = content_digest(&bytes);
            if digest != entry.digest {
                return Err(StasisError::ManifestVerification(format!(
                    "file '{name}' digest mismatch: expected {}, got {digest}",
                    entry.digest
                )));
            }
        }
    }

    // Check for untracked / extra files or forbidden symlinks in bak_dir (tamper detection)
    for entry in fs::read_dir(bak_dir).map_err(|e| StasisError::Io {
        context: format!("read dir {}", bak_dir.display()),
        source: e,
    })? {
        let entry = entry.map_err(|e| StasisError::Io {
            context: format!("read dir entry in {}", bak_dir.display()),
            source: e,
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_symlink() {
            return Err(StasisError::ManifestVerification(format!(
                "symlinks forbidden in backup: '{name}' (ADR-0007, ADR-0039)"
            )));
        }
        if !manifest.entries.contains_key(&name) {
            return Err(StasisError::ManifestVerification(format!(
                "untracked file '{name}' in backup (tamper detected)"
            )));
        }
    }

    Ok(())
}

fn restore_dir_recursive(src: &Path, dst: &Path) -> Result<(), StasisError> {
    if !dst.exists() {
        fs::create_dir_all(dst).map_err(|e| StasisError::Io {
            context: format!("create dir {}", dst.display()),
            source: e,
        })?;
        set_mode(dst, 0o700)?;
    }
    for entry in fs::read_dir(src).map_err(|e| StasisError::Io {
        context: format!("read dir {}", src.display()),
        source: e,
    })? {
        let entry = entry.map_err(|e| StasisError::Io {
            context: format!("read dir entry in {}", src.display()),
            source: e,
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&src_path).map_err(|e| StasisError::Io {
            context: format!("stat {}", src_path.display()),
            source: e,
        })?;
        if meta.file_type().is_symlink() {
            return Err(StasisError::ManifestVerification(format!(
                "refusing symlink at {} during restore (ADR-0007)",
                src_path.display()
            )));
        } else if meta.is_dir() {
            restore_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|e| StasisError::Io {
                context: format!("copy {} to {}", src_path.display(), dst_path.display()),
                source: e,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                let _ = fs::set_permissions(&dst_path, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn compute_dir_digest(dir: &Path) -> Result<String, std::io::Error> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            let sub_digest = compute_dir_digest(&entry.path())?;
            entries.push(format!("dir:{name}:{sub_digest}"));
        } else {
            let bytes = fs::read(entry.path())?;
            let file_digest = content_digest(&bytes);
            entries.push(format!("file:{name}:{file_digest}"));
        }
    }
    entries.sort();
    let combined = entries.join("\n");
    Ok(content_digest(combined.as_bytes()))
}

fn read_parcel_blueprint_info(tree: &HamletTree, uid: u32) -> Option<(SafeName, SafeName, u32)> {
    let title_path = tree.parcel_title(uid);
    if let Ok(content) = fs::read_to_string(&title_path) {
        if let Ok(doc) = TitleDoc::parse(&content) {
            for entry in &doc.entries {
                if let TitleEntry::Provision {
                    archetype,
                    blueprint,
                    version,
                    ..
                } = entry
                {
                    return Some((archetype.clone(), blueprint.clone(), *version));
                }
            }
        }
    }
    let deed_path = tree.parcel_deed(uid);
    if let Ok(content) = fs::read_to_string(&deed_path) {
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("- blueprint:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(bp_str) = parts.first() {
                    let version = parts
                        .get(1)
                        .and_then(|v| v.strip_prefix('v'))
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(1);
                    if let Ok(bp_name) = sanitize_name(bp_str) {
                        return Some((bp_name.clone(), bp_name, version));
                    }
                }
            }
        }
    }
    None
}
