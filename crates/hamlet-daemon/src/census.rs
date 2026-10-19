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
//!    [`MountSet::for_agent`] (SPEC §4) via the [`Runtime`] seam
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

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{AgentRef, Charter, CharterError, ParcelLease};
use hamlet_core::config::Config;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::SlotError;
use hamlet_core::tree::HamletTree;

use crate::bootstrap::{
    BootstrapError, BootstrapReport, BootstrapTarget, Bootstrapper, ImageOutcome,
};
use crate::hostperm::{set_mode, HostPermError, HostPermissions, MODE_PARCEL_DIR, MODE_PARCEL_DOC};
use crate::podman::{PodmanError, RunSpec, Runtime};

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
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    format!(
        "# TITLE — chain of title · parcel{uid}\n\
         \n\
         Chronological, auditable lineage of every trigger that opened or reshaped\n\
         this lease (SPEC §10). Timestamps are lineage data only — never state\n\
         inputs (CONVENTIONS §3 rule 4).\n\
         \n\
         1. trigger: `provision_parcel(identity=\"{identity}\", commission={commission})` · unix {unix_ts} · slot {uid} allocated sequentially and permanently (SPEC §4)\n"
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
        write_stub(
            &self.tree.parcel_title(uid),
            &title_stub(identity, uid, commission, unix_now()),
        )?;
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
}
