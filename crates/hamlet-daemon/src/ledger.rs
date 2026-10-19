//! Guild ledger governor: commission creation, bare git repositories,
//! `--seed` import, roster management, and CLI query views (SPEC §9.1;
//! ADR-0036, ADR-0037).
//!
//! # Responsibilities (Sprint 06 Step 02)
//!
//! 1. **Commission creation (`SPEC §9.1`)**:
//!    - Sanitize `<slug>` via [`hamlet_core::sanitize::sanitize_name`].
//!    - Validate the `COMMISSION.md` brief (non-empty, LF-only, bounded by
//!      [`hamlet_core::packet::MAX_ARTIFACT_BYTES`]).
//!    - Validate optional `--seed <dir>` before any mutation (must be a real
//!      directory; symlinks and special files anywhere in the seed tree are
//!      refused under ADR-0007).
//!    - Allocate the next sequential 1-based commission ID from
//!      [`Charter::create_commission`] in `Draft` state (ADR-0036 cl. 1–2:
//!      file-as-truth — `Draft` transitions to `Active` when `INDENTURE.md`
//!      is posted in Step 03).
//!    - Materialize `guild/ledger/commission<n>/` and initialize a bare git
//!      repository at `guild/ledger/commission<n>/git/` (`HEAD ->
//!      refs/heads/main`, `core.hooksPath = /dev/null` per ADR-0037 cl. 4,
//!      world-readable `0755`/`0644` modes per ADR-0011).
//!    - If `--seed <dir>` is given, import regular files into an isolated
//!      staging tree and commit them as the initial commit on `refs/heads/main`
//!      authored by `hamlet-daemon`, with a `TITLE`-style lineage body.
//!    - Write `guild/ledger/commission<n>/COMMISSION.md` via `create_new`
//!      (`0644`, ADR-0007).
//!    - Persist `charter.json` atomically (`Charter::save_atomic`). Any
//!      failure before charter persistence unwinds the newly created
//!      `commission<n>/` directory.
//!    - Construct [`WakeEvent::from_commission`] and fire the
//!      [`CommissionWake`] hook (`WakeReason::CommissionCreated`).
//! 2. **Roster management (`ADR-0036 cl. 1–2`)**:
//!    - [`Ledger::assign_uid`] and [`Ledger::release_uid`] mutate both the
//!      commission's `roster` and the slot's `commission` binding atomically
//!      in `charter.json`, feeding Sprint 04 freight ACL audits
//!      ([`crate::freight::FreightGuard::policy`]) and Sprint 05 decree
//!      validation ([`crate::decrees::DecreeExecutor`]).
//! 3. **Tree-derived queries (`hamlet commission list` / `show`)**:
//!    - Read strictly from `charter.json` and the `.hamlet/` tree — no hidden
//!      daemon state, no wall-clock fields in rendered views.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::charter::{Charter, CharterError};
use hamlet_core::commission::{
    parse_deliverable_notice_subject, parse_entry_notice_subject, CommissionError, CommissionEvent,
    CommissionState, IndentureAck, IndentureAckStatus, IndentureBlock, IndentureDoc,
    IndentureEntryStatus,
};
use hamlet_core::decree::{
    DecreeError, DecreeId, EscalationNotice, TreeRelPath, ESCALATION_VERSION,
};
use hamlet_core::gitmed::{
    agent_branch, agent_branch_ref, agent_identity_email, is_bundle_name, is_own_agent_branch_ref,
    BundleRejection, MAX_BUNDLE_BYTES, MAX_DIFF_BODY_BYTES,
};
use hamlet_core::mail::{Mail, MailError, MailHeader, MailSender, SYSTEM_SENDER};
use hamlet_core::packet::MAX_ARTIFACT_BYTES;
use hamlet_core::sanitize::{sanitize_name, SafeName, SanitizeError};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use serde::{Deserialize, Serialize};

use crate::hostperm::{MODE_PARCEL_DOC, MODE_SHARED_READONLY};
use crate::mailroute::{MailActor, MailRouteError, MailRouter};
use crate::mayor::{MayorEngine, MayorError, WakeEvent};
use crate::podman::{PodmanError, Runtime};
use crate::promote::{DeedPromoter, PromotionOutcome, PromotionTrigger};
use crate::telemetry::{AgentKey, Telemetry, TraceRecord};
use crate::watch::Dispatcher;

/// Mode for `COMMISSION.md` (`0644`: daemon-owned, world-readable across userns).
const MODE_COMMISSION_DOC: u32 = 0o644;

/// Default maximum `INDENTURE.md` validation retries before escalating to a
/// human and transitioning the commission to `Failed` (SPEC §9.2).
pub const DEFAULT_MAX_INDENTURE_RETRIES: u32 = 3;

/// Quarantine file name for a rejected `INDENTURE.md` inside
/// `guild/ledger/commission<n>/` so `Draft` retains `has_indenture == false`
/// (ADR-0036 cl. 2) while preserving the rejected artifact for audit and
/// escalation evidence.
pub const INDENTURE_REJECTED_FILENAME: &str = ".indenture.rejected";

/// Author name recorded on daemon-authored `--seed` commits (SPEC §9.1).
pub const SEED_AUTHOR_NAME: &str = "hamlet-daemon";

/// Author email recorded on daemon-authored `--seed` commits (SPEC §9.1).
pub const SEED_AUTHOR_EMAIL: &str = "daemon@hamlet.local";

/// Default branch name for every commission bare repository.
pub const DEFAULT_BRANCH: &str = "main";

/// Default `HEAD` symbolic ref target inside `guild/ledger/commission<n>/git/`.
pub const DEFAULT_HEAD_REF: &str = "refs/heads/main";

/// Canonical Git empty-tree SHA-1 OID used as `base` when diffing the first
/// push to a bare repository whose `main` branch is still unborn.
pub const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Dispatcher handler name for the git bundle push mediation plane (ADR-0037).
pub const HANDLER_NAME_GIT_SPOOL: &str = "git_spool";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed failure modes of the guild ledger governor.
#[derive(Debug)]
pub enum LedgerError {
    /// The `.hamlet/` village has not been initialized (`charter.json` missing).
    UninitializedVillage {
        /// Village root path that lacked `charter.json`.
        root: PathBuf,
    },
    /// Commission slug failed [`sanitize_name`].
    Sanitize(SanitizeError),
    /// The `COMMISSION.md` brief was empty, contained CR bytes, or exceeded
    /// [`MAX_ARTIFACT_BYTES`].
    InvalidBrief {
        /// Why the brief was refused.
        reason: String,
    },
    /// A path in the village tree or `--seed` directory is a symlink (refused
    /// under ADR-0007).
    SymlinkRefused {
        /// Offending symlink path.
        path: PathBuf,
    },
    /// The `--seed <dir>` argument is missing, not a directory, or contains
    /// non-regular files (FIFOs, sockets, devices).
    SeedDirInvalid {
        /// Offending path.
        path: PathBuf,
        /// Why it was refused.
        reason: String,
    },
    /// A target file in the ledger already existed or was not a regular file.
    TargetExists {
        /// Offending path.
        path: PathBuf,
    },
    /// Charter load, validation, duplicate-slug, or roster error.
    Charter(CharterError),
    /// No commission with the requested slug exists in `charter.json`.
    UnknownCommissionSlug {
        /// The slug that was queried.
        slug: SafeName,
    },
    /// `git` subprocess failed or could not be spawned.
    Git {
        /// Description of the git step.
        step: String,
        /// Captured stderr or OS error text.
        detail: String,
    },
    /// Constructing a Mayor wake event failed.
    Wake(MayorError),
    /// Composing or delivering an indenture mail failed.
    Mail(String),
    /// Constructing an escalation notice failed.
    Decree(DecreeError),
    /// Filesystem I/O failure with path/operation context.
    Io {
        /// Operation and path that failed.
        context: String,
        /// Underlying OS error.
        source: io::Error,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UninitializedVillage { root } => write!(
                f,
                "village at {} is not initialized (missing charter.json; run `hamlet init` first)",
                root.display()
            ),
            Self::Sanitize(e) => write!(f, "invalid commission slug: {e}"),
            Self::InvalidBrief { reason } => write!(f, "invalid commission brief: {reason}"),
            Self::SymlinkRefused { path } => {
                write!(f, "refusing symlink at {} (ADR-0007)", path.display())
            }
            Self::SeedDirInvalid { path, reason } => {
                write!(f, "invalid seed directory {}: {reason}", path.display())
            }
            Self::TargetExists { path } => {
                write!(
                    f,
                    "ledger target {} already exists (refusing to clobber)",
                    path.display()
                )
            }
            Self::Charter(e) => write!(f, "{e}"),
            Self::UnknownCommissionSlug { slug } => {
                write!(f, "unknown commission slug '{slug}'")
            }
            Self::Git { step, detail } => write!(f, "git {step} failed: {detail}"),
            Self::Wake(e) => write!(f, "commission wake construction failed: {e}"),
            Self::Mail(msg) => write!(f, "indenture mail delivery failed: {msg}"),
            Self::Decree(e) => write!(f, "escalation construction failed: {e}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sanitize(e) => Some(e),
            Self::Charter(e) => Some(e),
            Self::Wake(e) => Some(e),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<SanitizeError> for LedgerError {
    fn from(e: SanitizeError) -> Self {
        Self::Sanitize(e)
    }
}

impl From<CharterError> for LedgerError {
    fn from(e: CharterError) -> Self {
        Self::Charter(e)
    }
}

impl From<CommissionError> for LedgerError {
    fn from(e: CommissionError) -> Self {
        Self::Charter(CharterError::from(e))
    }
}

impl From<MailError> for LedgerError {
    fn from(e: MailError) -> Self {
        Self::Mail(e.to_string())
    }
}

impl From<MailRouteError> for LedgerError {
    fn from(e: MailRouteError) -> Self {
        Self::Mail(e.to_string())
    }
}

impl From<DecreeError> for LedgerError {
    fn from(e: DecreeError) -> Self {
        Self::Decree(e)
    }
}

fn io_err(context: &str) -> impl FnOnce(io::Error) -> LedgerError + '_ {
    move |source| LedgerError::Io {
        context: context.to_owned(),
        source,
    }
}

// ---------------------------------------------------------------------------
// CommissionWake seam
// ---------------------------------------------------------------------------

/// Hook fired when the guild ledger plane emits a Mayor wake event
/// (`CommissionCreated`, `IndentureRejected`, `AllEntriesDone`, `GitDiffReady`;
/// SPEC §9.1–§9.3, ADR-0037).
pub trait CommissionWake: Send + Sync {
    /// Notify the Mayor plane that `commission` was created with `event`
    /// (`WakeReason::CommissionCreated`, carrying the `COMMISSION.md`
    /// artifact).
    fn commission_created(&self, commission: u64, event: &WakeEvent);

    /// Notify the Mayor plane that `INDENTURE.md` on `commission` failed
    /// schema or roster validation on `attempt` (`WakeReason::IndentureRejected`).
    fn indenture_rejected(&self, _commission: u64, _attempt: u32, _event: &WakeEvent) {}

    /// Notify the Mayor plane that all indenture entries on `commission`
    /// reached `Done` and the commission transitioned `Active -> WindingDown`
    /// (`WakeReason::AllEntriesDone`).
    fn all_entries_done(&self, _commission: u64, _event: &WakeEvent) {}

    /// Notify the Mayor plane that a validated git bundle advanced an agent's
    /// work branch in `commission<n>/git/` (`WakeReason::GitDiffReady`,
    /// ADR-0037 cl. 7).
    fn git_diff_ready(&self, _commission: u64, _event: &WakeEvent) {}

    /// Notify the Mayor plane that an agent submitted a deliverable for review
    /// (`WakeReason::DeliverableSubmitted`, SPEC §7, §9.4).
    fn deliverable_submitted(&self, _commission: u64, _event: &WakeEvent) {}
}

/// Default no-op wake hook used when no in-process [`MayorEngine`] is wired
/// (e.g. standalone CLI invocation against a dormant tree).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCommissionWake;

impl CommissionWake for NoopCommissionWake {
    fn commission_created(&self, _commission: u64, _event: &WakeEvent) {}
}

/// Reaction spy for tests: records every `(commission_id, WakeEvent)` fired by
/// [`Ledger`] in arrival order.
#[derive(Debug, Default)]
pub struct RecordingCommissionWake {
    events: Mutex<Vec<(u64, WakeEvent)>>,
}

impl RecordingCommissionWake {
    /// Construct an empty wake spy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all recorded `(commission_id, WakeEvent)` pairs.
    #[must_use]
    pub fn events(&self) -> Vec<(u64, WakeEvent)> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Number of wakes recorded so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// True when no wakes have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CommissionWake for RecordingCommissionWake {
    fn commission_created(&self, commission: u64, event: &WakeEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((commission, event.clone()));
    }

    fn indenture_rejected(&self, commission: u64, _attempt: u32, event: &WakeEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((commission, event.clone()));
    }

    fn all_entries_done(&self, commission: u64, event: &WakeEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((commission, event.clone()));
    }

    fn git_diff_ready(&self, commission: u64, event: &WakeEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((commission, event.clone()));
    }

    fn deliverable_submitted(&self, commission: u64, event: &WakeEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((commission, event.clone()));
    }
}

/// Bridges [`CommissionWake`] to a live [`MayorEngine`] on the current Tokio
/// runtime (mirroring [`crate::mayor::MayorMailBridge`]).
#[derive(Clone)]
pub struct CommissionWakeBridge {
    engine: Arc<MayorEngine>,
}

impl CommissionWakeBridge {
    /// Bridge an engine into the guild ledger plane.
    #[must_use]
    pub fn new(engine: Arc<MayorEngine>) -> Self {
        Self { engine }
    }

    fn spawn_wake(&self, event: &WakeEvent) {
        let engine = Arc::clone(&self.engine);
        let event = event.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = engine.wake(event).await {
                    eprintln!("hamlet-daemon: mayor commission wake failed: {e}");
                }
            });
        }
    }
}

impl CommissionWake for CommissionWakeBridge {
    fn commission_created(&self, _commission: u64, event: &WakeEvent) {
        self.spawn_wake(event);
    }

    fn indenture_rejected(&self, _commission: u64, _attempt: u32, event: &WakeEvent) {
        self.spawn_wake(event);
    }

    fn all_entries_done(&self, _commission: u64, event: &WakeEvent) {
        self.spawn_wake(event);
    }

    fn git_diff_ready(&self, _commission: u64, event: &WakeEvent) {
        self.spawn_wake(event);
    }

    fn deliverable_submitted(&self, _commission: u64, event: &WakeEvent) {
        self.spawn_wake(event);
    }
}

// ---------------------------------------------------------------------------
// Pure lineage, git mediation & report types
// ---------------------------------------------------------------------------

/// Build the `TITLE`-style lineage commit message for a daemon-authored
/// `--seed <dir>` initial commit on `main` (SPEC §9.1, §10).
///
/// Timestamps are lineage data only — never a state input (CONVENTIONS §3 r4).
#[must_use]
pub fn seed_commit_message(
    commission: u64,
    slug: &SafeName,
    file_count: usize,
    unix_ts: u64,
) -> String {
    format!(
        "seed: commission{commission} ({slug})\n\
         \n\
         # TITLE — chain of title · commission{commission}\n\
         \n\
         1. trigger: `commission_new(slug=\"{slug}\", commission={commission}, seeded_files={file_count})` · unix {unix_ts} · initial commit on `{DEFAULT_BRANCH}` authored by {SEED_AUTHOR_NAME} (SPEC §9.1)\n"
    )
}

/// Build the `.gitconfig` seeded into `/home/<uid>/.gitconfig` at parcel
/// provisioning so commits inside `/home/<uid>/work/` carry the slot's
/// required identity (`<uid>@hamlet.local`, ADR-0037 cl. 3).
#[must_use]
pub fn agent_gitconfig(uid: u32, identity: &SafeName) -> String {
    let email = agent_identity_email(uid);
    format!(
        "[user]\n\
         \tname = {identity} (parcel{uid})\n\
         \temail = {email}\n\
         [safe]\n\
         \tdirectory = /var/git\n\
         [init]\n\
         \tdefaultBranch = {DEFAULT_BRANCH}\n\
         [commit]\n\
         \tgpgsign = false\n"
    )
}

/// Build the `hamlet-git-push` helper script installed at
/// `/home/<uid>/.pixi/bin/hamlet-git-push` (`0755`) during parcel provisioning
/// (`ADR-0037 cl. 1–3`).
///
/// Usage inside `/home/<uid>/work/`:
/// - `hamlet-git-push` (bundles the current `HEAD` branch `agent/<uid>[/<topic>]`
///   into `/home/<uid>/git-spool/<name>.bundle` via atomic temp+rename)
/// - `hamlet-git-push <topic-or-branch> [<bundle-name>.bundle]`
#[must_use]
pub fn git_push_helper_script(uid: u32) -> String {
    let base_branch = agent_branch(uid);
    let base_ref = agent_branch_ref(uid);
    format!(
        "#!/bin/sh\n\
         set -eu\n\
         SPOOL=\"/home/{uid}/git-spool\"\n\
         mkdir -p -m 0700 \"$SPOOL\"\n\
         TARGET=\"\"\n\
         NAME=\"\"\n\
         if [ $# -eq 1 ]; then\n\
           case \"$1\" in\n\
             *.bundle)\n\
               NAME=\"$1\"\n\
               ;;\n\
             refs/heads/*|agent/*)\n\
               TARGET=\"$1\"\n\
               ;;\n\
             *)\n\
               if git show-ref --verify --quiet \"{base_ref}/$1\" 2>/dev/null; then\n\
                 TARGET=\"{base_ref}/$1\"\n\
               else\n\
                 NAME=\"$1\"\n\
               fi\n\
               ;;\n\
           esac\n\
         elif [ $# -ge 2 ]; then\n\
           TARGET=\"$1\"\n\
           NAME=\"$2\"\n\
         fi\n\
         if [ -z \"$TARGET\" ]; then\n\
           REF=$(git symbolic-ref -q HEAD || true)\n\
           if [ -z \"$REF\" ]; then\n\
             REF=\"{base_ref}\"\n\
           fi\n\
         else\n\
           case \"$TARGET\" in\n\
             refs/heads/*) REF=\"$TARGET\" ;;\n\
             agent/*) REF=\"refs/heads/$TARGET\" ;;\n\
             *) REF=\"{base_ref}/$TARGET\" ;;\n\
           esac\n\
         fi\n\
         case \"$REF\" in\n\
           {base_ref}|{base_ref}/*) ;;\n\
           *)\n\
             echo \"hamlet-git-push: branch ref '$REF' must be '{base_branch}' or '{base_branch}/<topic>'\" >&2\n\
             exit 1\n\
             ;;\n\
         esac\n\
         if [ -z \"$NAME\" ]; then\n\
           TS=$(date -u +%Y%m%dT%H%M%SZ 2>/dev/null || echo 0)\n\
           NAME=\"push-${{TS}}-$$.bundle\"\n\
         else\n\
           case \"$NAME\" in\n\
             *.bundle) ;;\n\
             *) NAME=\"${{NAME}}.bundle\" ;;\n\
           esac\n\
         fi\n\
         TMP=\"$SPOOL/.staging-$$.tmp\"\n\
         rm -f \"$TMP\"\n\
         git bundle create \"$TMP\" \"$REF\"\n\
         chmod 0600 \"$TMP\"\n\
         mv -f \"$TMP\" \"$SPOOL/$NAME\"\n\
         printf '%s\\n' \"$SPOOL/$NAME\"\n"
    )
}

/// Seed `/home/<uid>/git-spool/`, `/home/<uid>/.gitconfig`, and
/// `/home/<uid>/.pixi/bin/hamlet-git-push` inside the agent's container at
/// provisioning time (`ADR-0037`).
///
/// # Errors
///
/// Returns [`PodmanError`] if any in-container `exec` step fails.
pub async fn seed_agent_git_home(
    runtime: &dyn Runtime,
    identity: &SafeName,
    uid: u32,
) -> Result<(), PodmanError> {
    let container = identity.as_str();
    let home = format!("/home/{uid}");
    let spool = format!("{home}/git-spool");
    let bin_dir = format!("{home}/.pixi/bin");
    let gitconfig_path = format!("{home}/.gitconfig");
    let helper_path = format!("{bin_dir}/hamlet-git-push");

    let mkdir_cmd = vec!["mkdir".to_owned(), "-p".to_owned(), spool.clone(), bin_dir];
    let out = runtime.exec(container, &mkdir_cmd, None).await?;
    if out.code != 0 {
        return Err(PodmanError::invalid(format!(
            "mkdir git-spool failed (exit {}): {}",
            out.code,
            out.stderr_lossy()
        )));
    }

    let chmod_spool = vec!["chmod".to_owned(), "0700".to_owned(), spool];
    let out = runtime.exec(container, &chmod_spool, None).await?;
    if out.code != 0 {
        return Err(PodmanError::invalid(format!(
            "chmod 0700 git-spool failed (exit {}): {}",
            out.code,
            out.stderr_lossy()
        )));
    }

    let gitconfig = agent_gitconfig(uid, identity);
    let tee_gitconfig = vec!["tee".to_owned(), gitconfig_path.clone()];
    let out = runtime
        .exec(container, &tee_gitconfig, Some(gitconfig.as_bytes()))
        .await?;
    if out.code != 0 {
        return Err(PodmanError::invalid(format!(
            "write .gitconfig failed (exit {}): {}",
            out.code,
            out.stderr_lossy()
        )));
    }

    let chmod_gitconfig = vec!["chmod".to_owned(), "0600".to_owned(), gitconfig_path];
    let out = runtime.exec(container, &chmod_gitconfig, None).await?;
    if out.code != 0 {
        return Err(PodmanError::invalid(format!(
            "chmod 0600 .gitconfig failed (exit {}): {}",
            out.code,
            out.stderr_lossy()
        )));
    }

    let helper = git_push_helper_script(uid);
    let tee_helper = vec!["tee".to_owned(), helper_path.clone()];
    let out = runtime
        .exec(container, &tee_helper, Some(helper.as_bytes()))
        .await?;
    if out.code != 0 {
        return Err(PodmanError::invalid(format!(
            "write hamlet-git-push failed (exit {}): {}",
            out.code,
            out.stderr_lossy()
        )));
    }

    let chmod_helper = vec!["chmod".to_owned(), "0755".to_owned(), helper_path];
    let out = runtime.exec(container, &chmod_helper, None).await?;
    if out.code != 0 {
        return Err(PodmanError::invalid(format!(
            "chmod 0755 hamlet-git-push failed (exit {}): {}",
            out.code,
            out.stderr_lossy()
        )));
    }

    Ok(())
}

/// Build the commission completion-signalling and git-mediation addendum
/// appended to an agent's initial session brief at provisioning
/// (`SPEC §9.2–§9.3`, `ADR-0037`).
#[must_use]
pub fn agent_commission_brief_addendum(commission: u64, uid: u32, identity: &SafeName) -> String {
    let branch = agent_branch(uid);
    let email = agent_identity_email(uid);
    format!(
        "\n--- COMMISSION {commission} PROTOCOL (parcel{uid} · {identity}) ---\n\
         - Commission brief: `/var/ledger/COMMISSION.md`\n\
         - Indenture contract: `/var/ledger/INDENTURE.md` (`## {identity}`)\n\
         - Central bare git repository: `/var/git` (clone `/var/git` into `/home/{uid}/work/`, commit on `{branch}` or `{branch}/<topic>` as `{email}`, and run `hamlet-git-push` to stage `<name>.bundle` in `/home/{uid}/git-spool/`)\n\
         - Progress notice (optional): write a mail to `/var/mail/1000/` with `From: {uid}`, `To: 1000`, `Subject: commission:{commission}:in_progress`\n\
         - Completion notice (required when `done` criteria are met): write a mail to `/var/mail/1000/` with `From: {uid}`, `To: 1000`, `Subject: commission:{commission}:done`\n"
    )
}

/// Build the `PROPRIETOR.md` runtime mutation section recording an agent's
/// delivered indenture contract on `commission<n>` (`SPEC §9.2`, `ADR-0037`).
#[must_use]
pub fn proprietor_indenture_section(commission: u64, uid: u32, block: &IndentureBlock) -> String {
    let agent = &block.agent;
    let branch = agent_branch(uid);
    format!(
        "\n### Commission {commission} Indenture (`## {agent}`)\n\
         - goal: {}\n\
         - scope: {}\n\
         - done: {}\n\
         - context: `/var/ledger/COMMISSION.md`, `/var/ledger/INDENTURE.md`, `/var/git`, `/var/freight/`\n\
         - git_workflow: clone `/var/git` into `/home/{uid}/work/`, commit on `{branch}` (or `{branch}/<topic>`) as `{uid}@hamlet.local`, stage `<name>.bundle` in `/home/{uid}/git-spool/` via `hamlet-git-push`\n\
         - completion_signal: mail `To: {MAYOR_UID}` (`From: {uid}`) with `Subject: commission:{commission}:done` when the `done` criterion is satisfied (optional `Subject: commission:{commission}:in_progress` when starting work)\n",
        block.goal, block.scope, block.done
    )
}

/// Compose the daemon-authored (`From: system`) mail message delivering an
/// agent's indenture entry, git bundle mediation workflow, and
/// completion-signalling instructions (`SPEC §9.2`, `ADR-0037`).
///
/// # Errors
///
/// Returns [`MailError`] if the recipient or body violates mail invariants.
pub fn compose_indenture_mail(
    commission: u64,
    slug: &SafeName,
    uid: u32,
    block: &IndentureBlock,
) -> Result<Mail, MailError> {
    let from =
        MailSender::Name(
            sanitize_name(SYSTEM_SENDER).map_err(|_| MailError::InvalidFrom {
                value: SYSTEM_SENDER.to_owned(),
            })?,
        );
    let subject = format!("commission:{commission}:indenture ({slug})");
    let branch = agent_branch(uid);
    let email = agent_identity_email(uid);
    let body = format!(
        "You are assigned to commission{commission} ({slug}) under `## {agent}`.\n\
         \n\
         ## Indenture Contract\n\
         - goal: {goal}\n\
         - scope: {scope}\n\
         - done: {done}\n\
         \n\
         ## Commission Context\n\
         - Brief: `guild/ledger/commission{commission}/COMMISSION.md` (mounted at `/var/ledger/COMMISSION.md`)\n\
         - Indenture: `guild/ledger/commission{commission}/INDENTURE.md` (mounted at `/var/ledger/INDENTURE.md`)\n\
         - Bare Git Repo: `guild/ledger/commission{commission}/git` (mounted at `/var/git`)\n\
         - Freight Bay: `courier/freight/` (mounted at `/var/freight/`)\n\
         \n\
         ## Git Work-Branch Loop (ADR-0037)\n\
         - Clone `/var/git` into `/home/{uid}/work/` (`git clone /var/git /home/{uid}/work`).\n\
         - Create and commit on your own branch (`git checkout -b {branch}` or `{branch}/<topic>`) using your seeded identity (`{email}`).\n\
         - Stage bundles into `/home/{uid}/git-spool/<name>.bundle` via `hamlet-git-push` (or `git bundle create`).\n\
         - The daemon validates your bundle, updates `refs/heads/{branch}` in `guild/ledger/commission{commission}/git/`, and writes `/home/{uid}/git-spool/<name>.bundle.ack`.\n\
         \n\
         ## Completion Signalling\n\
         - When you begin active work, you may mail the Mayor (`To: {MAYOR_UID}`, `From: {uid}`) with:\n\
           `Subject: commission:{commission}:in_progress`\n\
         - When your `done` criterion is satisfied, you MUST mail the Mayor (`To: {MAYOR_UID}`, `From: {uid}`) with:\n\
           `Subject: commission:{commission}:done`\n",
        agent = block.agent,
        goal = block.goal,
        scope = block.scope,
        done = block.done,
    );
    let mail = Mail {
        header: MailHeader {
            from,
            to: uid,
            subject,
            date: format!("unix:{}", unix_now()),
        },
        body,
    };
    Mail::parse(&mail.render())
}

/// Disposition recorded in `/home/<uid>/git-spool/<name>.bundle.ack` (ADR-0037 cl. 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitBundleAckStatus {
    /// Bundle passed all ADR-0037 checks, updated the agent's branch in
    /// `commission<n>/git/`, archived to `keep/census/archives/git/<n>/<uid>/`,
    /// and woke the Mayor with `git_diff_ready`.
    Accepted,
    /// Bundle failed an ADR-0037 check; renamed to `<name>.bundle.<token>` and
    /// refused with a diagnostic mail to `<uid>`.
    Rejected,
}

/// Daemon-written acknowledgment at `/home/<uid>/git-spool/<name>.bundle.ack`
/// (`ADR-0037 cl. 6`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBundleAck {
    /// Commission ID (`0` when rejected before a commission could be resolved).
    pub commission: u64,
    /// Slot UID whose spool staged the bundle.
    pub uid: u32,
    /// Original bundle file name (`<name>.bundle`).
    pub bundle: String,
    /// Disposition (`accepted` or `rejected`).
    pub status: GitBundleAckStatus,
    /// Updated branch ref (`refs/heads/agent/<uid>[/<topic>]`) when accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
    /// Previous commit OID (or empty-tree OID on first push to an unborn repo) when accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_oid: Option<String>,
    /// New branch tip commit OID when accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    /// Short rejection token (`oversize`, `verify`, `ref`, `identity`, `nonff`) when rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable diagnostic when rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl GitBundleAck {
    /// Construct an `accepted` bundle receipt.
    #[must_use]
    pub fn accepted(
        commission: u64,
        uid: u32,
        bundle: impl Into<String>,
        branch_ref: impl Into<String>,
        base_oid: impl Into<String>,
        head_oid: impl Into<String>,
    ) -> Self {
        Self {
            commission,
            uid,
            bundle: bundle.into(),
            status: GitBundleAckStatus::Accepted,
            branch_ref: Some(branch_ref.into()),
            base_oid: Some(base_oid.into()),
            head_oid: Some(head_oid.into()),
            reason: None,
            diagnostic: None,
        }
    }

    /// Construct a `rejected` bundle receipt.
    #[must_use]
    pub fn rejected(
        commission: u64,
        uid: u32,
        bundle: impl Into<String>,
        rejection: BundleRejection,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            commission,
            uid,
            bundle: bundle.into(),
            status: GitBundleAckStatus::Rejected,
            branch_ref: None,
            base_oid: None,
            head_oid: None,
            reason: Some(rejection.token().to_owned()),
            diagnostic: Some(diagnostic.into()),
        }
    }

    /// Render deterministic pretty JSON with trailing LF.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("GitBundleAck serializes");
        s.push('\n');
        s
    }

    /// Parse a rendered `.bundle.ack` file.
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` if the JSON is malformed.
    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }
}

/// Outcome of [`Ledger::process_git_bundle`] for a single spool file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitBundleOutcome {
    /// File name was not a `.bundle` candidate or vanished before processing.
    Ignored,
    /// Bundle was validated, applied to `commission<n>/git/`, archived to
    /// `keep/census/archives/git/<n>/<uid>/<name>`, acked, and `GitDiffReady`
    /// fired.
    Accepted {
        /// 1-based commission ID.
        commission: u64,
        /// Agent slot UID.
        uid: u32,
        /// Bundle file name (`<name>.bundle`).
        bundle: String,
        /// Full branch ref advanced (`refs/heads/agent/<uid>[/<topic>]`).
        branch_ref: String,
        /// Base OID used for the diff range.
        base_oid: String,
        /// New head OID on `branch_ref`.
        head_oid: String,
        /// Host path to the archived bundle under `keep/census/archives/git/`.
        archive_path: PathBuf,
        /// Constructed `GitDiffReady` wake event.
        wake: WakeEvent,
    },
    /// Bundle was refused under ADR-0037 cl. 3/6; renamed in place to
    /// `<name>.bundle.<token>`, `.ack` written, refusal mailed to `<uid>`, and
    /// sled trace recorded.
    Rejected {
        /// Commission ID (`0` if `uid` had no active commission).
        commission: u64,
        /// Agent slot UID.
        uid: u32,
        /// Bundle file name (`<name>.bundle`).
        bundle: String,
        /// Typed rejection reason (`Oversize`, `VerifyFailed`, `NotOwnBranch`,
        /// `ForeignIdentity`, `NonFastForward`).
        rejection: BundleRejection,
        /// Detailed diagnostic string.
        diagnostic: String,
        /// Host path to the renamed rejected bundle (`<name>.bundle.<token>`).
        rejected_path: PathBuf,
    },
}

/// Outcome of [`Ledger::reconcile_indenture`] for a single commission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndentureOutcome {
    /// No new `INDENTURE.md` required action (absent or already processed).
    Unchanged,
    /// `INDENTURE.md` validated, `Draft -> Active` applied, `.ack` written
    /// (`done`), and each rostered entry delivered and marked `Delivered`.
    Posted {
        /// 1-based commission ID.
        commission: u64,
        /// Rostered UIDs whose indenture contracts were delivered.
        delivered_uids: Vec<u32>,
    },
    /// `INDENTURE.md` failed schema or roster validation; `.ack` written
    /// (`rejected`), `INDENTURE.md` quarantined to `.indenture.rejected`, and
    /// the Mayor re-woken with `WakeReason::IndentureRejected`.
    Rejected {
        /// 1-based commission ID.
        commission: u64,
        /// 1-based validation failure attempt counter.
        attempt: u32,
        /// Validation error summary delivered to the Mayor.
        error: String,
    },
    /// Validation retries exhausted: `.ack` written (`escalated`),
    /// `commission<n>` transitioned `Draft -> Failed` via
    /// `CommissionEvent::EscalationFatal`, and an `EscalationNotice` filed in
    /// `keep/census/manor/escalations/`.
    Escalated {
        /// 1-based commission ID.
        commission: u64,
        /// Total attempts exhausted.
        attempt: u32,
        /// Host path to the escalation file written under `manor/escalations/`.
        escalation_path: PathBuf,
        /// Final validation error summary.
        error: String,
    },
}

/// Outcome of [`Ledger::create_commission`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedCommission {
    /// Sequential 1-based commission ID allocated in `charter.json`.
    pub id: u64,
    /// Validated commission slug.
    pub slug: SafeName,
    /// Initial lifecycle state (`CommissionState::Draft` per ADR-0036).
    pub state: CommissionState,
    /// Host path to `guild/ledger/commission<id>/`.
    pub dir: PathBuf,
    /// Host path to `guild/ledger/commission<id>/git/` (bare repo).
    pub git_dir: PathBuf,
    /// Host path to `guild/ledger/commission<id>/COMMISSION.md`.
    pub commission_md: PathBuf,
    /// Number of regular files imported when `--seed <dir>` was supplied.
    pub seeded_files: Option<usize>,
    /// Commit OID on `refs/heads/main` when `--seed <dir>` was supplied.
    pub seeded_commit: Option<String>,
    /// The `CommissionCreated` wake event constructed from `COMMISSION.md`.
    pub wake: WakeEvent,
}

impl CreatedCommission {
    /// Deterministic, wall-clock-free human summary for `hamlet commission new`.
    #[must_use]
    pub fn summary(&self) -> String {
        let id = self.id;
        let slug = &self.slug;
        let state = self.state.kind_name();
        let seed_desc = match self.seeded_files {
            None => "none (empty bare repository)".to_owned(),
            Some(n) => format!("{n} file(s) imported on {DEFAULT_BRANCH}"),
        };
        format!(
            "hamlet commission new: commission{id} ({slug})\n\
             \x20 id:            {id}\n\
             \x20 slug:          {slug}\n\
             \x20 state:         {state}\n\
             \x20 ledger:        guild/ledger/commission{id}\n\
             \x20 git:           guild/ledger/commission{id}/git (bare, HEAD -> {DEFAULT_HEAD_REF})\n\
             \x20 brief:         guild/ledger/commission{id}/COMMISSION.md\n\
             \x20 seed:          {seed_desc}\n\
             \x20 mayor wake:    commission_created\n"
        )
    }
}

/// Outcome of [`Ledger::close_commission`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionCloseReport {
    /// 1-based commission ID.
    pub id: u64,
    /// Sanitized commission slug.
    pub slug: SafeName,
    /// Final lifecycle state (typically `CommissionState::Closed`).
    pub state: CommissionState,
    /// Number of rostered parcels parked/stopped.
    pub parcels_stopped: usize,
    /// Path to exported telemetry summary when exported.
    pub archives_exported: Option<PathBuf>,
}

impl CommissionCloseReport {
    /// Deterministic, wall-clock-free human summary for `hamlet commission close`.
    #[must_use]
    pub fn summary(&self) -> String {
        let id = self.id;
        let slug = &self.slug;
        let state = self.state.kind_name();
        let archives = match &self.archives_exported {
            Some(p) => p.display().to_string(),
            None => "none".to_string(),
        };
        format!(
            "hamlet commission close: commission{id} ({slug})\n\
             \x20 id:            {id}\n\
             \x20 slug:          {slug}\n\
             \x20 state:         {state}\n\
             \x20 parcels stopped: {}\n\
             \x20 archives:      {archives}\n",
            self.parcels_stopped
        )
    }
}

/// One row of `hamlet commission list`, derived strictly from `charter.json`
/// and `.hamlet/guild/ledger/commission<id>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionSummary {
    /// 1-based commission ID.
    pub id: u64,
    /// Sanitized commission slug.
    pub slug: SafeName,
    /// Lifecycle state in `charter.json`.
    pub state: CommissionState,
    /// Rostered subagent UIDs in ascending order.
    pub roster: BTreeSet<u32>,
    /// Whether `guild/ledger/commission<id>/COMMISSION.md` exists as a file.
    pub has_brief: bool,
    /// Whether `guild/ledger/commission<id>/INDENTURE.md` exists as a file.
    pub has_indenture: bool,
    /// Whether `guild/ledger/commission<id>/git/HEAD` exists as a file.
    pub has_bare_git: bool,
}

/// Render a slice of [`CommissionSummary`] rows as a deterministic ASCII table
/// for `hamlet commission list`.
#[must_use]
pub fn render_commission_list(items: &[CommissionSummary]) -> String {
    let headers = ["ID", "SLUG", "STATE", "ROSTER", "BRIEF", "INDENTURE", "GIT"];
    if items.is_empty() {
        return format!(
            "{}  {}  {}  {}  {}  {}  {}\n(no commissions)\n",
            headers[0], headers[1], headers[2], headers[3], headers[4], headers[5], headers[6]
        );
    }

    let rows: Vec<[String; 7]> = items
        .iter()
        .map(|item| {
            let roster = if item.roster.is_empty() {
                "-".to_owned()
            } else {
                item.roster
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            };
            [
                item.id.to_string(),
                item.slug.to_string(),
                item.state.kind_name().to_owned(),
                roster,
                if item.has_brief { "yes" } else { "no" }.to_owned(),
                if item.has_indenture { "yes" } else { "no" }.to_owned(),
                if item.has_bare_git { "bare" } else { "missing" }.to_owned(),
            ]
        })
        .collect();

    let mut widths = [0usize; 7];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    write_padded_row(&mut out, &headers.map(str::to_owned), &widths);
    for row in &rows {
        write_padded_row(&mut out, row, &widths);
    }
    out
}

fn write_padded_row(out: &mut String, cells: &[String; 7], widths: &[usize; 7]) {
    for (i, cell) in cells.iter().enumerate() {
        if i + 1 == cells.len() {
            out.push_str(cell);
        } else {
            out.push_str(cell);
            let pad = widths[i].saturating_sub(cell.len()) + 2;
            for _ in 0..pad {
                out.push(' ');
            }
        }
    }
    out.push('\n');
}

/// One rostered subagent entry in [`CommissionDetail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterMember {
    /// Permanent subagent UID (`1001..=1999`).
    pub uid: u32,
    /// Bound agent identity from `charter.slots`, if present.
    pub identity: Option<SafeName>,
}

/// One indenture entry status row in [`CommissionDetail`] (`SPEC §9.3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndentureEntryView {
    /// Permanent subagent UID (`1001..=1999`).
    pub uid: u32,
    /// Bound agent identity from `charter.slots`, if present.
    pub identity: Option<SafeName>,
    /// Current entry status in `charter.json` (`pending` / `delivered` /
    /// `in_progress` / `done`).
    pub status: IndentureEntryStatus,
}

/// Detailed view of a single commission for `hamlet commission show <slug>`,
/// derived strictly from `charter.json` and `.hamlet/` tree files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionDetail {
    /// 1-based commission ID.
    pub id: u64,
    /// Sanitized commission slug.
    pub slug: SafeName,
    /// Lifecycle state in `charter.json`.
    pub state: CommissionState,
    /// Rostered subagent slots in ascending UID order.
    pub roster: Vec<RosterMember>,
    /// Per-agent indenture entry statuses in ascending UID order.
    pub indenture_entries: Vec<IndentureEntryView>,
    /// Symbolic ref target in `git/HEAD` (e.g. `refs/heads/main`), if present.
    pub git_head: Option<String>,
    /// Commit OID at `refs/heads/main`, if `main` has been seeded/advanced.
    pub main_oid: Option<String>,
    /// Verbatim contents of `COMMISSION.md`, if present on disk.
    pub brief: Option<String>,
    /// Whether `INDENTURE.md` is present on disk.
    pub has_indenture: bool,
}

impl CommissionDetail {
    /// Deterministic, wall-clock-free rendering for `hamlet commission show`.
    #[must_use]
    pub fn render(&self) -> String {
        let id = self.id;
        let slug = &self.slug;
        let state = self.state.kind_name();
        let roster_str = if self.roster.is_empty() {
            "none".to_owned()
        } else {
            self.roster
                .iter()
                .map(|m| match &m.identity {
                    Some(ident) => format!("{} ({ident})", m.uid),
                    None => m.uid.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let git_status = match (&self.git_head, &self.main_oid) {
            (Some(head), Some(_)) => format!("HEAD -> {head}, {DEFAULT_BRANCH}: seeded"),
            (Some(head), None) => format!("HEAD -> {head}, {DEFAULT_BRANCH}: unborn"),
            (None, _) => "missing".to_owned(),
        };
        let brief_status = if self.brief.is_some() {
            "present"
        } else {
            "absent"
        };
        let indenture_status = if self.has_indenture {
            "present"
        } else {
            "absent"
        };

        let mut out = format!(
            "commission: commission{id} ({slug})\n\
             \x20 id:         {id}\n\
             \x20 slug:       {slug}\n\
             \x20 state:      {state}\n\
             \x20 roster:     {roster_str}\n\
             \x20 ledger:     guild/ledger/commission{id}\n\
             \x20 git:        guild/ledger/commission{id}/git ({git_status})\n\
             \x20 brief:      guild/ledger/commission{id}/COMMISSION.md ({brief_status})\n\
             \x20 indenture:  guild/ledger/commission{id}/INDENTURE.md ({indenture_status})\n"
        );
        if !self.indenture_entries.is_empty() {
            let entries_str = self
                .indenture_entries
                .iter()
                .map(|e| match &e.identity {
                    Some(ident) => format!("{} ({ident}): {}", e.uid, e.status.as_str()),
                    None => format!("{}: {}", e.uid, e.status.as_str()),
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("  entries:    {entries_str}\n"));
        }
        if let Some(brief) = &self.brief {
            out.push_str("\n--- COMMISSION.md ---\n");
            out.push_str(brief);
            if !brief.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Ledger governor
// ---------------------------------------------------------------------------

/// The guild ledger governor over one `.hamlet/` village tree.
pub struct Ledger {
    tree: HamletTree,
    wake: Arc<dyn CommissionWake>,
    mail: Option<Arc<MailRouter>>,
    telemetry: Option<Arc<Telemetry>>,
    promoter: Option<Arc<DeedPromoter>>,
    promotion_trigger: PromotionTrigger,
    podman_bin: PathBuf,
    max_indenture_retries: u32,
    charter_lock: Mutex<()>,
    mail_seq: Mutex<u64>,
}

impl fmt::Debug for Ledger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ledger")
            .field("tree", &self.tree)
            .field("max_indenture_retries", &self.max_indenture_retries)
            .field("promotion_trigger", &self.promotion_trigger)
            .finish_non_exhaustive()
    }
}

impl Ledger {
    /// Construct a ledger governor over `tree` with [`NoopCommissionWake`].
    #[must_use]
    pub fn new(tree: HamletTree) -> Self {
        Self {
            tree,
            wake: Arc::new(NoopCommissionWake),
            mail: None,
            telemetry: None,
            promoter: None,
            promotion_trigger: PromotionTrigger::EntryCompletion,
            podman_bin: PathBuf::from("podman"),
            max_indenture_retries: DEFAULT_MAX_INDENTURE_RETRIES,
            charter_lock: Mutex::new(()),
            mail_seq: Mutex::new(1),
        }
    }

    /// Attach a [`DeedPromoter`] for evolving blueprints from DEED alterations (`SPEC §9.4`).
    #[must_use]
    pub fn with_promoter(mut self, promoter: Arc<DeedPromoter>) -> Self {
        self.promoter = Some(promoter);
        self
    }

    /// Configure the promotion trigger ([`PromotionTrigger::EntryCompletion`] or
    /// [`PromotionTrigger::CommissionWindDown`]).
    #[must_use]
    pub fn with_promotion_trigger(mut self, trigger: PromotionTrigger) -> Self {
        self.promotion_trigger = trigger;
        self
    }

    /// Borrow the attached [`DeedPromoter`] if one is installed.
    #[must_use]
    pub fn promoter(&self) -> Option<&Arc<DeedPromoter>> {
        self.promoter.as_ref()
    }

    /// Attach a [`CommissionWake`] hook (e.g. [`RecordingCommissionWake`] in
    /// tests or [`CommissionWakeBridge`] in the live daemon).
    #[must_use]
    pub fn with_wake(mut self, wake: Arc<dyn CommissionWake>) -> Self {
        self.wake = wake;
        self
    }

    /// Attach a [`MailRouter`] used to deliver per-agent indenture contracts
    /// and git bundle refusal diagnostics (`SPEC §9.2`, `ADR-0037 cl. 6`).
    /// When absent, the ledger writes the canonical `.mail` spool file
    /// directly to `courier/mail/<uid>/`.
    #[must_use]
    pub fn with_mail_router(mut self, mail: Arc<MailRouter>) -> Self {
        self.mail = Some(mail);
        self
    }

    /// Attach a [`Telemetry`] sink for sled trace logging of git bundle
    /// mediation events (`ADR-0037 cl. 6`).
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Override the `podman` binary used for `podman unshare` fallbacks when
    /// accessing `0700` subuid-owned parcel git spools (`ADR-0011`, `ADR-0037`).
    #[must_use]
    pub fn with_podman_bin(mut self, podman_bin: impl Into<PathBuf>) -> Self {
        self.podman_bin = podman_bin.into();
        self
    }

    /// Configure the maximum `INDENTURE.md` validation failures before
    /// escalating to a human and failing the commission (`SPEC §9.2`;
    /// defaults to [`DEFAULT_MAX_INDENTURE_RETRIES`]).
    #[must_use]
    pub fn with_max_indenture_retries(mut self, max_retries: u32) -> Self {
        self.max_indenture_retries = max_retries.max(1);
        self
    }

    /// Borrow the governed [`HamletTree`].
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Create a new commission (`SPEC §9.1`):
    ///
    /// 1. Sanitize `raw_slug` via [`sanitize_name`].
    /// 2. Validate `brief` (non-empty, LF-only, ≤ [`MAX_ARTIFACT_BYTES`]).
    /// 3. Validate `seed_dir` (if `Some`): real directory, no symlinks or
    ///    non-regular files anywhere in its tree (ADR-0007).
    /// 4. Allocate sequential 1-based commission ID `n` in `Draft` state
    ///    ([`Charter::create_commission`]).
    /// 5. Materialize `guild/ledger/commission<n>/` and initialize a bare git
    ///    repository at `guild/ledger/commission<n>/git/` (`HEAD ->
    ///    refs/heads/main`, `core.hooksPath = /dev/null`).
    /// 6. If `seed_dir` is provided, import its regular files as the initial
    ///    commit on `refs/heads/main` authored by `hamlet-daemon` with a
    ///    `TITLE`-style lineage body.
    /// 7. Write `guild/ledger/commission<n>/COMMISSION.md` via `create_new`
    ///    (`0644`).
    /// 8. Save `charter.json` atomically (`Charter::save_atomic`). Any failure
    ///    before charter commit rolls back `guild/ledger/commission<n>/`.
    /// 9. Construct [`WakeEvent::from_commission`] and fire the
    ///    [`CommissionWake`] hook.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] on invalid slug/brief/seed dir, duplicate slug,
    /// existing ledger directory, git failure, charter failure, or I/O error.
    pub fn create_commission(
        &self,
        raw_slug: &str,
        brief: &str,
        seed_dir: Option<&Path>,
    ) -> Result<CreatedCommission, LedgerError> {
        // --- 1. Pre-flight validation (zero side effects) -------------------
        let slug = sanitize_name(raw_slug)?;
        validate_brief(brief)?;
        let seed_files = match seed_dir {
            Some(dir) => Some(collect_seed_files(dir)?),
            None => None,
        };

        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut charter = self.load_charter_checked()?;

        let id = charter.create_commission(slug.clone())?;
        let commission_dir = self.tree.commission(id);
        let git_dir = self.tree.commission_git(id);
        let commission_md = self.tree.commission_md(id);

        if fs::symlink_metadata(&commission_dir).is_ok() {
            return Err(LedgerError::TargetExists {
                path: commission_dir,
            });
        }

        // --- 2. Materialize ledger dirs + bare git repo + COMMISSION.md -----
        let result = (|| -> Result<(Option<usize>, Option<String>), LedgerError> {
            create_dir_checked(&commission_dir, MODE_SHARED_READONLY)?;
            create_dir_checked(&git_dir, MODE_SHARED_READONLY)?;

            init_bare_repo(&git_dir)?;

            let (seeded_files, seeded_commit) = match &seed_files {
                Some(files) => {
                    let count = files.len();
                    let oid = import_seed_commit(&commission_dir, &git_dir, id, &slug, files)?;
                    (Some(count), Some(oid))
                }
                None => (None, None),
            };

            normalize_repo_permissions(&git_dir)?;
            write_commission_md(&commission_md, brief)?;

            charter.validate()?;
            charter.save_atomic(&self.tree.charter())?;
            Ok((seeded_files, seeded_commit))
        })();

        let (seeded_files, seeded_commit) = match result {
            Ok(pair) => pair,
            Err(err) => {
                let _ = fs::remove_dir_all(&commission_dir);
                return Err(err);
            }
        };

        // --- 3. Construct and fire CommissionCreated Mayor wake -------------
        let wake = WakeEvent::from_commission(&self.tree, id).map_err(LedgerError::Wake)?;
        self.wake.commission_created(id, &wake);

        Ok(CreatedCommission {
            id,
            slug,
            state: CommissionState::Draft,
            dir: commission_dir,
            git_dir,
            commission_md,
            seeded_files,
            seeded_commit,
            wake,
        })
    }

    // -----------------------------------------------------------------------
    // Roster management API (ADR-0036 cl. 1–2)
    // -----------------------------------------------------------------------

    /// Roster `uid` onto `commission` (`Draft` or `Active` only), persisting
    /// both `commissions[commission].roster` and `slots[uid].commission`
    /// atomically to `charter.json`.
    ///
    /// Feeds Sprint 04 freight ACL audits ([`crate::freight::FreightGuard::policy`])
    /// and Sprint 05 decree target validation ([`crate::decrees::DecreeExecutor`]).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::Charter`] if `commission` does not exist, its
    /// roster is frozen (`Completed`/`Archived`), `uid` has no slot record, or
    /// `uid` is already bound to a different commission.
    pub fn assign_uid(&self, commission: u64, uid: u32) -> Result<(), LedgerError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut charter = self.load_charter_checked()?;
        charter.assign_uid(commission, uid)?;
        charter.validate()?;
        charter.save_atomic(&self.tree.charter())?;
        Ok(())
    }

    /// Unroster `uid` from `commission` (`Draft` or `Active` only), clearing
    /// `commissions[commission].roster` and `slots[uid].commission` atomically
    /// in `charter.json`.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::Charter`] if `commission` does not exist, its
    /// roster is frozen (`Completed`/`Archived`), or `uid` is not rostered on
    /// `commission`.
    pub fn release_uid(&self, commission: u64, uid: u32) -> Result<(), LedgerError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut charter = self.load_charter_checked()?;
        charter.release_uid(commission, uid)?;
        charter.validate()?;
        charter.save_atomic(&self.tree.charter())?;
        Ok(())
    }

    /// Return the current rostered UIDs for `commission` from `charter.json`.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::Charter`] if `commission` does not exist or
    /// `charter.json` cannot be loaded.
    pub fn roster(&self, commission: u64) -> Result<BTreeSet<u32>, LedgerError> {
        let charter = self.load_charter_checked()?;
        let record =
            charter
                .commissions
                .get(&commission)
                .ok_or_else(|| CharterError::Commission {
                    message: format!("commission {commission} does not exist"),
                })?;
        Ok(record.roster.clone())
    }

    /// Apply a typed [`CommissionEvent`] to `commission` and persist
    /// `charter.json` atomically (`ADR-0036 cl. 1`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::Charter`] if the `(state, event)` transition is
    /// refused or `commission` does not exist.
    pub fn apply_event(&self, commission: u64, event: &CommissionEvent) -> Result<(), LedgerError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut charter = self.load_charter_checked()?;
        charter.apply_commission_event(commission, event)?;
        charter.validate()?;
        charter.save_atomic(&self.tree.charter())?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // INDENTURE decomposition + entry tracking (SPEC §9.2–§9.3)
    // -----------------------------------------------------------------------

    /// Scan all commissions in `charter.json` and reconcile any newly posted
    /// `guild/ledger/commission<n>/INDENTURE.md` files (`SPEC §9.2`).
    ///
    /// Returns every commission outcome that was not [`IndentureOutcome::Unchanged`].
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] on charter, I/O, or wake construction failure.
    pub fn reconcile_indentures(&self) -> Result<Vec<IndentureOutcome>, LedgerError> {
        let ids: Vec<u64> = {
            let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
            let charter = self.load_charter_checked()?;
            charter.commissions.keys().copied().collect()
        };
        let mut outcomes = Vec::new();
        for id in ids {
            let outcome = self.reconcile_indenture(id)?;
            if outcome != IndentureOutcome::Unchanged {
                outcomes.push(outcome);
            }
        }
        Ok(outcomes)
    }

    /// Reconcile `guild/ledger/commission<commission>/INDENTURE.md` (`SPEC §9.2`):
    ///
    /// 1. Validate `INDENTURE.md` against [`IndentureDoc::parse`] and verify
    ///    that every `## <agent>` block resolves to a provisioned slot in
    ///    `charter.slots` that is rostered on `commission`.
    /// 2. On invalid `INDENTURE.md`:
    ///    - Quarantine `INDENTURE.md` to `.indenture.rejected` so `Draft`
    ///      commissions never retain an unvalidated `INDENTURE.md`
    ///      (`ADR-0036 cl. 2`).
    ///    - If `attempt < max_indenture_retries`: write a `rejected`
    ///      `INDENTURE.md.ack` and fire [`CommissionWake::indenture_rejected`]
    ///      (`WakeReason::IndentureRejected`).
    ///    - If `attempt >= max_indenture_retries`: transition the commission
    ///      `Draft -> Failed` via [`CommissionEvent::EscalationFatal`], write
    ///      an `escalated` `INDENTURE.md.ack`, and file an [`EscalationNotice`]
    ///      in `keep/census/manor/escalations/`.
    /// 3. On valid `INDENTURE.md`:
    ///    - Transition `commission` in `charter.json` via
    ///      [`CommissionEvent::IndenturePosted`] (`Draft -> Active`, all
    ///      entries initialized to [`IndentureEntryStatus::Pending`]).
    ///    - Write a `done` `INDENTURE.md.ack`.
    ///    - Append each agent's indenture contract to `parcel<uid>/PROPRIETOR.md`
    ///      (if present) and deliver a daemon-authored (`From: system`) mail
    ///      carrying the contract, context refs, and completion-signalling
    ///      instructions.
    ///    - Advance each delivered entry in `charter.json`
    ///      (`Pending -> Delivered`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] if a symlink is encountered, `charter.json`
    /// cannot be updated, mail delivery fails, or wake construction fails.
    pub fn reconcile_indenture(&self, commission: u64) -> Result<IndentureOutcome, LedgerError> {
        let indenture_path = self.tree.commission_indenture(commission);
        match fs::symlink_metadata(&indenture_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(LedgerError::SymlinkRefused {
                    path: indenture_path,
                });
            }
            Ok(meta) if meta.is_file() => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(IndentureOutcome::Unchanged);
            }
            Ok(_) | Err(_) => return Ok(IndentureOutcome::Unchanged),
        }

        let raw_bytes = fs::read(&indenture_path)
            .map_err(io_err(&format!("read {}", indenture_path.display())))?;
        let digest = format!("{:016x}", fnv1a_64(&raw_bytes));
        let ack_path = self.tree.commission_indenture_ack(commission);
        let prev_ack = read_indenture_ack(&ack_path)?;

        if let Some(ack) = &prev_ack {
            if (ack.status == IndentureAckStatus::Done && ack.digest == digest)
                || ack.status == IndentureAckStatus::Escalated
            {
                return Ok(IndentureOutcome::Unchanged);
            }
        }

        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut charter = self.load_charter_checked()?;
        let Some(record) = charter.commissions.get(&commission).cloned() else {
            return Ok(IndentureOutcome::Unchanged);
        };

        if record.state() != CommissionState::Draft {
            if record.state() == CommissionState::Active && prev_ack.is_none() {
                write_indenture_ack(&ack_path, &IndentureAck::done(commission, &digest))?;
            }
            return Ok(IndentureOutcome::Unchanged);
        }

        match validate_indenture_for_commission(&raw_bytes, commission, &record.roster, &charter) {
            Ok(resolved) => {
                let entries: BTreeSet<u32> = resolved.keys().copied().collect();
                charter.apply_commission_event(
                    commission,
                    &CommissionEvent::IndenturePosted { entries },
                )?;
                charter.validate()?;
                charter.save_atomic(&self.tree.charter())?;

                write_indenture_ack(&ack_path, &IndentureAck::done(commission, &digest))?;

                let mut delivered_uids = Vec::with_capacity(resolved.len());
                for (&uid, block) in &resolved {
                    append_proprietor_indenture(&self.tree, commission, uid, block)?;
                    self.deliver_indenture_mail(commission, &record.slug, uid, block)?;
                    charter.mark_indenture_entry(
                        commission,
                        uid,
                        IndentureEntryStatus::Delivered,
                    )?;
                    delivered_uids.push(uid);
                }
                charter.validate()?;
                charter.save_atomic(&self.tree.charter())?;

                Ok(IndentureOutcome::Posted {
                    commission,
                    delivered_uids,
                })
            }
            Err(error) => {
                let attempt = prev_ack
                    .as_ref()
                    .map_or(1, |ack| ack.attempt.saturating_add(1));
                let rejected_path = self
                    .tree
                    .commission(commission)
                    .join(INDENTURE_REJECTED_FILENAME);
                quarantine_rejected_indenture(&indenture_path, &rejected_path)?;

                if attempt >= self.max_indenture_retries {
                    charter
                        .apply_commission_event(commission, &CommissionEvent::EscalationFatal)?;
                    charter.validate()?;
                    charter.save_atomic(&self.tree.charter())?;

                    let ack = IndentureAck::escalated(commission, attempt, &digest, &error);
                    write_indenture_ack(&ack_path, &ack)?;
                    let escalation_path = write_indenture_escalation(
                        &self.tree, commission, attempt, &digest, &error,
                    )?;

                    Ok(IndentureOutcome::Escalated {
                        commission,
                        attempt,
                        escalation_path,
                        error,
                    })
                } else {
                    let ack = IndentureAck::rejected(commission, attempt, &digest, &error);
                    write_indenture_ack(&ack_path, &ack)?;
                    drop(_guard);

                    let wake_body = format!(
                        "commission{commission} INDENTURE.md rejected (attempt {attempt} of {max}):\n\
                         {error}\n\
                         Fix the errors above and rewrite `guild/ledger/commission{commission}/INDENTURE.md`.\n",
                        max = self.max_indenture_retries,
                    );
                    let wake = WakeEvent::from_indenture_rejected(commission, attempt, wake_body)
                        .map_err(LedgerError::Wake)?;
                    self.wake.indenture_rejected(commission, attempt, &wake);

                    Ok(IndentureOutcome::Rejected {
                        commission,
                        attempt,
                        error,
                    })
                }
            }
        }
    }

    /// Advance `uid`'s indenture entry on `commission` to `status`
    /// (`Pending -> Delivered -> InProgress -> Done`), and when all entries on
    /// `commission` reach `Done`, transition `commission` `Active -> WindingDown`
    /// and fire [`CommissionWake::all_entries_done`] (`SPEC §9.3`).
    ///
    /// Returns `Ok(true)` if this update caused `commission` to transition to
    /// `WindingDown` and fire the `AllEntriesDone` wake, or `Ok(false)`
    /// otherwise. Duplicate or already-passed statuses are treated
    /// idempotently (`Ok(false)`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] if `commission` or `uid` is unknown, the
    /// commission is not `Active`, or charter/wake operations fail.
    pub fn mark_indenture_entry(
        &self,
        commission: u64,
        uid: u32,
        status: IndentureEntryStatus,
    ) -> Result<bool, LedgerError> {
        let all_done = {
            let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut charter = self.load_charter_checked()?;
            let Some(record) = charter.commissions.get(&commission) else {
                return Err(CharterError::Commission {
                    message: format!("commission {commission} does not exist"),
                }
                .into());
            };
            if record.state() != CommissionState::Active {
                return Ok(false);
            }
            if let Some(&current) = record.indenture_entries.get(&uid) {
                if current >= status {
                    return Ok(false);
                }
            }
            charter.mark_indenture_entry(commission, uid, status)?;
            let now_all_done = charter
                .commissions
                .get(&commission)
                .is_some_and(|r| r.state() == CommissionState::Active && r.all_entries_done());
            if now_all_done {
                charter.apply_commission_event(commission, &CommissionEvent::AllEntriesDone)?;
            }
            charter.validate()?;
            charter.save_atomic(&self.tree.charter())?;
            now_all_done
        };

        if status == IndentureEntryStatus::Done
            && self.promotion_trigger == PromotionTrigger::EntryCompletion
        {
            let _ = self.promote_parcel_deed(uid);
        }

        if all_done {
            if self.promotion_trigger == PromotionTrigger::CommissionWindDown {
                let _ = self.promote_commission_deeds(commission);
            }
            let wake = WakeEvent::from_all_entries_done(&self.tree, commission)
                .map_err(LedgerError::Wake)?;
            self.wake.all_entries_done(commission, &wake);
        }
        Ok(all_done)
    }

    /// Promote alterations from `parcel<uid>/DEED.md` into the parent blueprint
    /// using the attached promoter (or a default promoter).
    pub fn promote_parcel_deed(&self, uid: u32) -> Result<PromotionOutcome, LedgerError> {
        let promoter = match &self.promoter {
            Some(p) => p.clone(),
            None => {
                let mut p = DeedPromoter::new(self.tree.clone());
                if let Some(tel) = &self.telemetry {
                    p = p.with_telemetry(tel.clone());
                }
                if let Some(mail) = &self.mail {
                    p = p.with_mail(mail.clone());
                }
                Arc::new(p)
            }
        };
        promoter.promote_parcel(uid).map_err(|e| LedgerError::Io {
            context: format!("promote parcel{uid} DEED"),
            source: io::Error::other(e.to_string()),
        })
    }

    /// Promote alterations for all parcels rostered on `commission`.
    pub fn promote_commission_deeds(
        &self,
        commission: u64,
    ) -> Result<Vec<PromotionOutcome>, LedgerError> {
        let promoter = match &self.promoter {
            Some(p) => p.clone(),
            None => {
                let mut p = DeedPromoter::new(self.tree.clone());
                if let Some(tel) = &self.telemetry {
                    p = p.with_telemetry(tel.clone());
                }
                if let Some(mail) = &self.mail {
                    p = p.with_mail(mail.clone());
                }
                Arc::new(p)
            }
        };
        promoter
            .promote_commission(commission)
            .map_err(|e| LedgerError::Io {
                context: format!("promote commission {commission} DEEDs"),
                source: io::Error::other(e.to_string()),
            })
    }

    /// Inspect a mail `Subject` addressed to the Mayor (`To: 1000`) from
    /// `sender_uid` and, if it matches the indenture notice convention
    /// (`commission:<n>:done` / `commission:<n>:in_progress`), advance the
    /// sender's entry in `charter.json` (`SPEC §9.3`).
    ///
    /// Returns `Ok(Some((commission, status, all_done)))` when `subject`
    /// matched the notice syntax, or `Ok(None)` for ordinary mail subjects.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] if `subject` matched an indenture notice but the
    /// charter transition or wake construction failed.
    pub fn handle_entry_notice(
        &self,
        sender_uid: u32,
        subject: &str,
    ) -> Result<Option<(u64, IndentureEntryStatus, bool)>, LedgerError> {
        let Some((commission, status)) = parse_entry_notice_subject(subject) else {
            return Ok(None);
        };
        let all_done = self.mark_indenture_entry(commission, sender_uid, status)?;
        Ok(Some((commission, status, all_done)))
    }

    /// Record an agent's deliverable submission for `commission`, transition
    /// `deliverable_in_review = true`, and fire the `DeliverableSubmitted` wake.
    pub fn submit_deliverable(
        &self,
        commission: u64,
        uid: u32,
        freight_path: Option<&str>,
        notes: Option<&str>,
    ) -> Result<(), LedgerError> {
        let _guard = self.charter_lock.lock().unwrap();
        let mut charter = Charter::load(&self.tree.charter())?;
        let record = charter.commissions.get_mut(&commission).ok_or_else(|| {
            LedgerError::Charter(CharterError::Invariant {
                message: format!("unknown commission id {commission}"),
            })
        })?;
        if !record.roster.contains(&uid) {
            return Err(LedgerError::from(
                hamlet_core::commission::CommissionError::EntryNotRostered { uid },
            ));
        }

        // If still Active, check if AllEntriesDone can be applied first
        if record.state == CommissionState::Active && record.all_entries_done() {
            record.apply_event(&CommissionEvent::AllEntriesDone)?;
        }

        record.apply_event(&CommissionEvent::DeliverableSubmitted)?;
        charter.save_atomic(&self.tree.charter())?;

        let body = notes.unwrap_or("deliverable submitted for review\n");
        let wake = WakeEvent::from_deliverable_submitted(commission, uid, freight_path, body)
            .map_err(LedgerError::Wake)?;
        self.wake.deliverable_submitted(commission, &wake);
        Ok(())
    }

    /// Inspect a mail `Subject` addressed to the Mayor (`To: 1000`) from
    /// `sender_uid` and, if it matches the deliverable submission convention
    /// (`commission:<n>:deliverable` / `commission:<n>:submit`), record the
    /// submission in `charter.json` and wake the Mayor with `DeliverableSubmitted`.
    pub fn handle_deliverable_notice(
        &self,
        sender_uid: u32,
        subject: &str,
        body: &str,
    ) -> Result<Option<u64>, LedgerError> {
        let Some((commission, subpath)) = parse_deliverable_notice_subject(subject) else {
            return Ok(None);
        };
        self.submit_deliverable(commission, sender_uid, subpath.as_deref(), Some(body))?;
        Ok(Some(commission))
    }

    /// Operator commission close (`hamlet commission close <slug>`):
    /// Advances commission to `WindingDown`, stops any running parcel sessions,
    /// advances state to `Closed` (`OperatorClose`), exports final telemetry
    /// archives, and persists `charter.json`.
    pub fn close_commission(&self, slug: &str) -> Result<CommissionCloseReport, LedgerError> {
        let safe_slug = sanitize_name(slug)?;
        let _guard = self.charter_lock.lock().unwrap();
        let mut charter = Charter::load(&self.tree.charter())?;
        let (&commission_id, record) = charter
            .commissions
            .iter_mut()
            .find(|(_, c)| c.slug == safe_slug)
            .ok_or_else(|| LedgerError::UnknownCommissionSlug {
                slug: safe_slug.clone(),
            })?;

        if record.state == CommissionState::Closed {
            return Ok(CommissionCloseReport {
                id: commission_id,
                slug: safe_slug,
                state: CommissionState::Closed,
                parcels_stopped: 0,
                archives_exported: None,
            });
        }

        let roster: Vec<u32> = record.roster.iter().copied().collect();
        let parcels_stopped = roster.len();

        // Mark parcels stasis in charter
        for &uid in &roster {
            if let Some(lease) = charter.parcels.get_mut(&uid) {
                lease.stasis = true;
            }
        }

        // Advance state via OperatorClose
        record.apply_event(&CommissionEvent::OperatorClose)?;
        charter.save_atomic(&self.tree.charter())?;

        // Export telemetry archives if telemetry available
        let mut archives_exported = None;
        if let Some(tel) = &self.telemetry {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if let Ok(path) = tel.export_commission_summary(&self.tree, commission_id, now_ms) {
                archives_exported = Some(path);
            }
            let _ = tel.prune_commission(commission_id);
        }

        Ok(CommissionCloseReport {
            id: commission_id,
            slug: safe_slug,
            state: CommissionState::Closed,
            parcels_stopped,
            archives_exported,
        })
    }

    fn deliver_indenture_mail(
        &self,
        commission: u64,
        slug: &SafeName,
        uid: u32,
        block: &IndentureBlock,
    ) -> Result<(), LedgerError> {
        let msg = compose_indenture_mail(commission, slug, uid, block)?;
        self.deliver_system_mail(uid, &msg.header.subject, &msg.body, &msg.header.date)
    }

    fn deliver_system_mail(
        &self,
        uid: u32,
        subject: &str,
        body: &str,
        date: &str,
    ) -> Result<(), LedgerError> {
        if let Some(router) = &self.mail {
            router.send(MailActor::System, uid, subject, body, Some(date))?;
            if !router.has_handler() {
                let recipient = sanitize_name(&uid.to_string())?;
                let _ = router.route_recipient(&recipient);
            }
            return Ok(());
        }

        // Standalone fallback when no MailRouter is wired: write the canonical
        // spool file atomically into `courier/mail/<uid>/`.
        let from =
            MailSender::Name(
                sanitize_name(SYSTEM_SENDER).map_err(|_| MailError::InvalidFrom {
                    value: SYSTEM_SENDER.to_owned(),
                })?,
            );
        let mail = Mail {
            header: MailHeader {
                from,
                to: uid,
                subject: subject.to_owned(),
                date: date.to_owned(),
            },
            body: body.to_owned(),
        };
        let rendered = mail.render();
        let seq = self.next_internal_seq();
        let fname = format!("{seq:012}-{SYSTEM_SENDER}-{seq:x}.mail");
        let recipient = sanitize_name(&uid.to_string())?;
        let dir = self.tree.courier_mail_recipient(&recipient);
        let final_path = dir.join(fname);
        if create_dir_checked(&dir, 0o700).is_ok()
            && write_atomic_file(&final_path, rendered.as_bytes(), MODE_PARCEL_DOC).is_ok()
        {
            return Ok(());
        }
        // Fallback through `podman unshare` when `courier/mail/<uid>/` is 0700
        // subuid-owned (ADR-0011).
        self.unshare_write_file(&dir, &final_path, rendered.as_bytes(), 0o600, uid)
    }

    fn next_internal_seq(&self) -> u64 {
        let mut guard = self.mail_seq.lock().unwrap_or_else(|e| e.into_inner());
        let current = *guard;
        *guard = current.saturating_add(1);
        current
    }

    // -----------------------------------------------------------------------
    // Git bundle push mediation (SPEC §14, ADR-0037)
    // -----------------------------------------------------------------------

    /// Scan all provisioned agent slots in `charter.json` and mediate any
    /// staged `.bundle` candidates in their `/home/<uid>/git-spool/`
    /// directories (`ADR-0037 cl. 1–7`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] if `charter.json` cannot be loaded or an
    /// unexpected I/O failure occurs.
    pub fn reconcile_git_spools(&self) -> Result<Vec<GitBundleOutcome>, LedgerError> {
        let uids: Vec<u32> = {
            let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
            let charter = self.load_charter_checked()?;
            charter
                .slots
                .keys()
                .copied()
                .filter(|&uid| uid != MAYOR_UID)
                .collect()
        };
        let mut outcomes = Vec::new();
        for uid in uids {
            let slot_outcomes = self.reconcile_git_spool(uid)?;
            outcomes.extend(slot_outcomes);
        }
        Ok(outcomes)
    }

    /// Scan `/home/<uid>/git-spool/` (`tree.parcel_git_spool(uid)`) in
    /// lexicographical order and mediate every `.bundle` candidate (`ADR-0037`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] on charter or fatal I/O errors.
    pub fn reconcile_git_spool(&self, uid: u32) -> Result<Vec<GitBundleOutcome>, LedgerError> {
        let candidates = self.list_spool_bundle_candidates(uid)?;
        let mut outcomes = Vec::new();
        for name in candidates {
            let outcome = self.process_git_bundle(uid, &name)?;
            if outcome != GitBundleOutcome::Ignored {
                outcomes.push(outcome);
            }
        }
        Ok(outcomes)
    }

    /// Validate and either apply or refuse a single `<bundle_name>` staged in
    /// `/home/<uid>/git-spool/` (`ADR-0037 cl. 1–7`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] on fatal charter, git, or filesystem errors.
    pub fn process_git_bundle(
        &self,
        uid: u32,
        bundle_name: &str,
    ) -> Result<GitBundleOutcome, LedgerError> {
        self.process_git_bundle_with_expected_owner(uid, bundle_name, None)
    }

    /// Like [`Ledger::process_git_bundle`], with an explicit expected file
    /// owner UID override for host-side unit tests exercising wrong-UID
    /// rejection (`BundleRejection::ForeignIdentity`) without root/chown.
    ///
    /// When `expected_owner_uid` is `None`, the bundle file's owner UID must
    /// match the spool directory's owner UID (or `uid` inside `podman unshare`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] on fatal charter, git, or filesystem errors.
    pub fn process_git_bundle_with_expected_owner(
        &self,
        uid: u32,
        bundle_name: &str,
        expected_owner_uid: Option<u32>,
    ) -> Result<GitBundleOutcome, LedgerError> {
        if !is_bundle_name(bundle_name) {
            return Ok(GitBundleOutcome::Ignored);
        }

        let commission_opt = {
            let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
            let charter = self.load_charter_checked()?;
            charter.slots.get(&uid).and_then(|s| s.commission)
        };
        let Some(commission) = commission_opt else {
            let spool_dir = self.tree.parcel_git_spool(uid);
            if !self.spool_entry_exists(&spool_dir.join(bundle_name)) {
                return Ok(GitBundleOutcome::Ignored);
            }
            return self.reject_git_bundle(
                0,
                uid,
                bundle_name,
                BundleRejection::NotOwnBranch,
                format!("slot {uid} is not assigned to a commission in charter.json"),
            );
        };

        let commission_dir = self.tree.commission(commission);
        let commission_git = self.tree.commission_git(commission);
        if !is_regular_file(&commission_git.join("HEAD")) {
            let spool_dir = self.tree.parcel_git_spool(uid);
            if !self.spool_entry_exists(&spool_dir.join(bundle_name)) {
                return Ok(GitBundleOutcome::Ignored);
            }
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::VerifyFailed,
                format!("commission{commission} bare git repository is missing"),
            );
        }

        let seq = self.next_internal_seq();
        let scratch_dir = commission_dir.join(format!(
            ".gitmed-scratch-{uid}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&scratch_dir);
        create_dir_checked(&scratch_dir, 0o700)?;

        let result = self.mediate_staged_bundle(
            commission,
            uid,
            bundle_name,
            expected_owner_uid,
            &commission_git,
            &scratch_dir,
        );
        let _ = fs::remove_dir_all(&scratch_dir);
        result
    }

    /// Register the git-spool reaction handler on `dispatcher` (`ADR-0037`).
    ///
    /// Register the git-spool reaction handler on `dispatcher` (`ADR-0037`).
    ///
    /// Handles [`Reaction::GitBundle`] by scanning `/home/<uid>/git-spool/` in
    /// deterministic lexicographical order.
    pub fn attach_git_spool(
        ledger: Arc<Self>,
        dispatcher: &Dispatcher,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = dispatcher.register(HANDLER_NAME_GIT_SPOOL);
        tokio::spawn(async move {
            loop {
                let Some(item) = rx.recv().await else {
                    break;
                };
                if let Reaction::GitBundle { uid, .. } = item.reaction {
                    if let Err(e) = ledger.reconcile_git_spool(uid) {
                        eprintln!("hamlet-daemon: git spool pass for uid {uid} failed: {e}");
                    }
                }
            }
        })
    }

    /// Run the startup git-spool and indenture recovery scans and attach the
    /// reaction handler to `dispatcher` (`ADR-0019`, `ADR-0037`).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] if the startup recovery pass fails.
    pub fn spawn_git_spool_plane(
        ledger: Arc<Self>,
        dispatcher: &Dispatcher,
    ) -> Result<tokio::task::JoinHandle<()>, LedgerError> {
        let _ = ledger.reconcile_indentures()?;
        let _ = ledger.reconcile_git_spools()?;
        Ok(Self::attach_git_spool(ledger, dispatcher))
    }

    fn mediate_staged_bundle(
        &self,
        commission: u64,
        uid: u32,
        bundle_name: &str,
        expected_owner_uid: Option<u32>,
        commission_git: &Path,
        scratch_dir: &Path,
    ) -> Result<GitBundleOutcome, LedgerError> {
        let staged_bundle = scratch_dir.join("candidate.bundle");
        match self.inspect_and_stage_spool_bundle(
            uid,
            bundle_name,
            expected_owner_uid,
            &staged_bundle,
        )? {
            SpoolStageResult::Missing => return Ok(GitBundleOutcome::Ignored),
            SpoolStageResult::Rejected(rejection, diagnostic) => {
                return self.reject_git_bundle(commission, uid, bundle_name, rejection, diagnostic);
            }
            SpoolStageResult::Staged => {}
        }

        // Build an isolated quarantine bare repo sharing existing objects via
        // `objects/info/alternates` and existing refs copied from `commission_git`
        // so `commission<n>/git/` is NEVER mutated unless every check passes.
        let scratch_repo = scratch_dir.join("quarantine.git");
        prepare_quarantine_repo(commission_git, &scratch_repo)?;

        // (ii) `git bundle verify` clean + `git bundle list-heads`
        let bundle_arg = staged_bundle.to_string_lossy().into_owned();
        if let Err(err) = run_git(
            &scratch_repo,
            None,
            None,
            &["bundle", "verify", &bundle_arg],
            "bundle verify",
        ) {
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::VerifyFailed,
                format!("git bundle verify failed: {err}"),
            );
        }

        let heads_out = match run_git(
            &scratch_repo,
            None,
            None,
            &["bundle", "list-heads", &bundle_arg],
            "bundle list-heads",
        ) {
            Ok(out) => out,
            Err(err) => {
                return self.reject_git_bundle(
                    commission,
                    uid,
                    bundle_name,
                    BundleRejection::VerifyFailed,
                    format!("git bundle list-heads failed: {err}"),
                );
            }
        };

        let heads: Vec<(&str, &str)> = heads_out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .filter_map(|l| l.split_once(char::is_whitespace))
            .map(|(oid, r)| (oid.trim(), r.trim()))
            .collect();

        if heads.is_empty() {
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::VerifyFailed,
                "bundle lists zero ref heads".to_owned(),
            );
        }

        // (iii) Branch namespace check: single ref under `refs/heads/agent/<uid>[/<topic>]`
        if heads.len() != 1 {
            let listed: Vec<&str> = heads.iter().map(|(_, r)| *r).collect();
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::NotOwnBranch,
                format!(
                    "bundle carries {} refs ({}); exactly one ref under {} is permitted",
                    heads.len(),
                    listed.join(", "),
                    agent_branch_ref(uid)
                ),
            );
        }

        let (_listed_oid, target_ref_str) = heads[0];
        let target_ref = target_ref_str.to_owned();
        if !is_own_agent_branch_ref(uid, &target_ref) {
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::NotOwnBranch,
                format!(
                    "bundle ref '{target_ref}' is not owned by uid {uid} (expected '{}' or '{}/<topic>')",
                    agent_branch_ref(uid),
                    agent_branch_ref(uid)
                ),
            );
        }

        // Unpack candidate into the quarantine repo to inspect commit ancestry
        // and author/committer identities without touching `commission<n>/git/`.
        let fetch_spec = format!("+{target_ref}:refs/quarantine/candidate");
        if let Err(err) = run_git(
            &scratch_repo,
            None,
            None,
            &["fetch", "--no-tags", &bundle_arg, &fetch_spec],
            "fetch bundle into quarantine",
        ) {
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::VerifyFailed,
                format!("quarantine unpack failed: {err}"),
            );
        }

        let head_oid = match run_git(
            &scratch_repo,
            None,
            None,
            &[
                "rev-parse",
                "--verify",
                "refs/quarantine/candidate^{commit}",
            ],
            "rev-parse candidate commit",
        ) {
            Ok(oid) => oid.trim().to_owned(),
            Err(err) => {
                return self.reject_git_bundle(
                    commission,
                    uid,
                    bundle_name,
                    BundleRejection::VerifyFailed,
                    format!("candidate ref does not resolve to a commit: {err}"),
                );
            }
        };

        // (v) Fast-forward check: must strictly advance current `target_ref`
        // tip, or `refs/heads/main` on first push when `main` exists.
        let existing_base = read_ref_oid(commission_git, &target_ref)
            .or_else(|| read_ref_oid(commission_git, DEFAULT_HEAD_REF));

        if let Some(ref base) = existing_base {
            if base == &head_oid {
                return self.reject_git_bundle(
                    commission,
                    uid,
                    bundle_name,
                    BundleRejection::NonFastForward,
                    format!("bundle head {head_oid} does not advance current tip {base}"),
                );
            }
            if run_git(
                &scratch_repo,
                None,
                None,
                &["merge-base", "--is-ancestor", base, &head_oid],
                "merge-base --is-ancestor",
            )
            .is_err()
            {
                return self.reject_git_bundle(
                    commission,
                    uid,
                    bundle_name,
                    BundleRejection::NonFastForward,
                    format!("bundle head {head_oid} is not a fast-forward of current tip {base}"),
                );
            }
        }

        // (iv) Commit author & committer identity check (`<uid>@hamlet.local`).
        let expected_email = agent_identity_email(uid);
        let rev_range = match &existing_base {
            Some(base) => format!("{base}..{head_oid}"),
            None => head_oid.clone(),
        };
        let rev_list = run_git(
            &scratch_repo,
            None,
            None,
            &[
                "rev-list",
                "--format=%H%x00%an%x00%ae%x00%cn%x00%ce",
                &rev_range,
            ],
            "rev-list identity audit",
        )?;

        let mut inspected_commits = 0usize;
        for line in rev_list.lines() {
            if line.starts_with("commit ") || line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\0').collect();
            if parts.len() != 5 {
                return self.reject_git_bundle(
                    commission,
                    uid,
                    bundle_name,
                    BundleRejection::ForeignIdentity,
                    "malformed commit identity header in bundle".to_owned(),
                );
            }
            inspected_commits += 1;
            let (c_oid, author_name, author_email, committer_name, committer_email) =
                (parts[0], parts[1], parts[2], parts[3], parts[4]);
            if author_name.trim().is_empty()
                || committer_name.trim().is_empty()
                || author_email != expected_email
                || committer_email != expected_email
            {
                return self.reject_git_bundle(
                    commission,
                    uid,
                    bundle_name,
                    BundleRejection::ForeignIdentity,
                    format!(
                        "commit {c_oid} carries author '{author_name} <{author_email}>' and \
                         committer '{committer_name} <{committer_email}>'; both must carry \
                         email <{expected_email}>"
                    ),
                );
            }
        }

        if inspected_commits == 0 {
            return self.reject_git_bundle(
                commission,
                uid,
                bundle_name,
                BundleRejection::NonFastForward,
                "bundle contains zero new commits".to_owned(),
            );
        }

        // --- All ADR-0037 checks passed: apply to `commission<n>/git/` ------
        let apply_spec = format!("{target_ref}:{target_ref}");
        run_git(
            commission_git,
            None,
            None,
            &["fetch", "--no-tags", &bundle_arg, &apply_spec],
            "apply verified bundle",
        )?;
        normalize_repo_permissions(commission_git)?;

        let base_for_diff = existing_base.unwrap_or_else(|| EMPTY_TREE_OID.to_owned());
        let unified_diff = run_git(
            commission_git,
            None,
            None,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                &base_for_diff,
                &head_oid,
            ],
            "diff unified",
        )?;
        let stat_summary = run_git(
            commission_git,
            None,
            None,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--stat",
                &base_for_diff,
                &head_oid,
            ],
            "diff --stat",
        )?;

        let diff_body = if !unified_diff.trim().is_empty()
            && (unified_diff.len() as u64) <= MAX_DIFF_BODY_BYTES
        {
            unified_diff
        } else if !stat_summary.trim().is_empty()
            && (stat_summary.len() as u64) <= MAX_DIFF_BODY_BYTES
        {
            stat_summary
        } else {
            format!("{base_for_diff}..{head_oid} ({target_ref})\n")
        };

        let base_safe = sanitize_name(&base_for_diff)?;
        let head_safe = sanitize_name(&head_oid)?;
        let wake = WakeEvent::from_git_diff(commission, base_safe, head_safe, diff_body)
            .map_err(LedgerError::Wake)?;

        // Archive bundle under `keep/census/archives/git/<commission>/<uid>/<bundle_name>`
        let archive_dir = self.tree.census_git_archive(commission, uid);
        create_dir_checked(&archive_dir, MODE_SHARED_READONLY)?;
        let archive_path = archive_dir.join(bundle_name);
        let staged_bytes = fs::read(&staged_bundle)
            .map_err(io_err(&format!("read staged {}", staged_bundle.display())))?;
        write_atomic_file(&archive_path, &staged_bytes, MODE_COMMISSION_DOC)?;

        // Remove `<name>.bundle` from spool and write `<name>.bundle.ack`
        let ack = GitBundleAck::accepted(
            commission,
            uid,
            bundle_name,
            &target_ref,
            &base_for_diff,
            &head_oid,
        );
        self.consume_accepted_spool_bundle(uid, bundle_name, &ack)?;

        self.record_git_trace(
            commission,
            uid,
            "git_bundle_accepted",
            format!("bundle={bundle_name} ref={target_ref} base={base_for_diff} head={head_oid}"),
        );
        self.wake.git_diff_ready(commission, &wake);

        Ok(GitBundleOutcome::Accepted {
            commission,
            uid,
            bundle: bundle_name.to_owned(),
            branch_ref: target_ref,
            base_oid: base_for_diff,
            head_oid,
            archive_path,
            wake,
        })
    }

    fn reject_git_bundle(
        &self,
        commission: u64,
        uid: u32,
        bundle_name: &str,
        rejection: BundleRejection,
        diagnostic: String,
    ) -> Result<GitBundleOutcome, LedgerError> {
        let ack = GitBundleAck::rejected(commission, uid, bundle_name, rejection, &diagnostic);
        let rejected_path =
            self.quarantine_rejected_spool_bundle(uid, bundle_name, rejection, &ack)?;

        let subject = format!(
            "commission:{commission}:git_bundle_rejected ({bundle_name} · {})",
            rejection.token()
        );
        let body = format!(
            "Your git bundle `{bundle_name}` in `/home/{uid}/git-spool/` was refused by the daemon.\n\
             \n\
             - reason: {} ({})\n\
             - diagnostic: {diagnostic}\n\
             - receipt: `/home/{uid}/git-spool/{bundle_name}.ack`\n\
             - quarantined_bundle: `/home/{uid}/git-spool/{bundle_name}.{}`\n",
            rejection.token(),
            rejection,
            rejection.token()
        );
        let date = format!("unix:{}", unix_now());
        let _ = self.deliver_system_mail(uid, &subject, &body, &date);

        self.record_git_trace(
            commission,
            uid,
            "git_bundle_rejected",
            format!(
                "bundle={bundle_name} reason={} diagnostic={diagnostic}",
                rejection.token()
            ),
        );

        Ok(GitBundleOutcome::Rejected {
            commission,
            uid,
            bundle: bundle_name.to_owned(),
            rejection,
            diagnostic,
            rejected_path,
        })
    }

    fn record_git_trace(&self, commission: u64, uid: u32, event: &str, detail: String) {
        if let Some(telemetry) = &self.telemetry {
            let record = TraceRecord {
                at_unix_ms: unix_now_ms(),
                event: event.to_owned(),
                detail,
            };
            if let Err(e) = telemetry.trace_event(AgentKey { commission, uid }, record) {
                eprintln!("hamlet-daemon: git mediation trace {event} failed: {e}");
            }
        }
    }

    fn list_spool_bundle_candidates(&self, uid: u32) -> Result<Vec<String>, LedgerError> {
        let spool_dir = self.tree.parcel_git_spool(uid);
        match fs::read_dir(&spool_dir) {
            Ok(entries) => {
                let mut names = Vec::new();
                for entry in entries {
                    let entry =
                        entry.map_err(io_err(&format!("read entry in {}", spool_dir.display())))?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if is_bundle_name(&name) {
                        names.push(name);
                    }
                }
                names.sort();
                Ok(names)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let out = Command::new(&self.podman_bin)
                    .args([
                        "unshare",
                        "sh",
                        "-c",
                        "if [ -d \"$1\" ] && [ ! -L \"$1\" ]; then ls -1A -- \"$1\"; fi",
                        "sh",
                        &spool_dir.to_string_lossy(),
                    ])
                    .output()
                    .map_err(io_err("podman unshare ls git-spool"))?;
                if !out.status.success() {
                    return Ok(Vec::new());
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                let mut names: Vec<String> = stdout
                    .lines()
                    .map(str::trim)
                    .filter(|n| is_bundle_name(n))
                    .map(str::to_owned)
                    .collect();
                names.sort();
                Ok(names)
            }
            Err(e) => Err(LedgerError::Io {
                context: format!("read_dir {}", spool_dir.display()),
                source: e,
            }),
        }
    }

    fn spool_entry_exists(&self, path: &Path) -> bool {
        match fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Command::new(&self.podman_bin)
                .args([
                    "unshare",
                    "sh",
                    "-c",
                    "[ -e \"$1\" ] || [ -L \"$1\" ]",
                    "sh",
                    &path.to_string_lossy(),
                ])
                .status()
                .is_ok_and(|s| s.success()),
            Err(_) => false,
        }
    }

    fn inspect_and_stage_spool_bundle(
        &self,
        uid: u32,
        bundle_name: &str,
        expected_owner_uid: Option<u32>,
        staged_dest: &Path,
    ) -> Result<SpoolStageResult, LedgerError> {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;

        let spool_dir = self.tree.parcel_git_spool(uid);
        let bundle_path = spool_dir.join(bundle_name);

        match fs::symlink_metadata(&spool_dir) {
            Ok(dir_meta) => {
                if dir_meta.file_type().is_symlink() || !dir_meta.file_type().is_dir() {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::ForeignIdentity,
                        format!(
                            "git-spool {} is a symlink or not a directory (ADR-0007)",
                            spool_dir.display()
                        ),
                    ));
                }
                let dir_mode = dir_meta.mode() & 0o7777;
                if (dir_mode & 0o022) != 0 {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::ForeignIdentity,
                        format!(
                            "git-spool {} has group/world-writable mode {:04o}",
                            spool_dir.display(),
                            dir_mode
                        ),
                    ));
                }

                let file_meta = match fs::symlink_metadata(&bundle_path) {
                    Ok(m) => m,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        return Ok(SpoolStageResult::Missing);
                    }
                    Err(e) => {
                        return Err(LedgerError::Io {
                            context: format!("lstat {}", bundle_path.display()),
                            source: e,
                        });
                    }
                };

                if file_meta.file_type().is_symlink() || !file_meta.file_type().is_file() {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::ForeignIdentity,
                        format!(
                            "bundle {} is a symlink or non-regular file (ADR-0007)",
                            bundle_path.display()
                        ),
                    ));
                }

                let file_mode = file_meta.mode() & 0o7777;
                if (file_mode & 0o022) != 0 {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::ForeignIdentity,
                        format!(
                            "bundle {} has group/world-writable mode {:04o}",
                            bundle_path.display(),
                            file_mode
                        ),
                    ));
                }

                let expected_uid = expected_owner_uid.unwrap_or_else(|| dir_meta.uid());
                if file_meta.uid() != expected_uid {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::ForeignIdentity,
                        format!(
                            "bundle {} owner uid {} does not match expected slot owner uid {}",
                            bundle_path.display(),
                            file_meta.uid(),
                            expected_uid
                        ),
                    ));
                }

                if file_meta.len() > MAX_BUNDLE_BYTES {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::Oversize,
                        format!(
                            "bundle {} is {} bytes, exceeding MAX_BUNDLE_BYTES ({MAX_BUNDLE_BYTES})",
                            bundle_name,
                            file_meta.len()
                        ),
                    ));
                }

                // Open with O_NOFOLLOW so a symlink planted between lstat and
                // open is refused by the kernel.
                let file = match OpenOptions::new()
                    .read(true)
                    .custom_flags(O_NOFOLLOW_FLAG)
                    .open(&bundle_path)
                {
                    Ok(f) => f,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        return Ok(SpoolStageResult::Missing);
                    }
                    Err(e) => {
                        return Ok(SpoolStageResult::Rejected(
                            BundleRejection::ForeignIdentity,
                            format!("open (O_NOFOLLOW) {} failed: {e}", bundle_path.display()),
                        ));
                    }
                };

                let mut dest = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(staged_dest)
                    .map_err(io_err(&format!("create staged {}", staged_dest.display())))?;

                let mut limited = file.take(MAX_BUNDLE_BYTES + 1);
                let copied = io::copy(&mut limited, &mut dest)
                    .map_err(io_err(&format!("stage {}", bundle_path.display())))?;
                if copied > MAX_BUNDLE_BYTES {
                    return Ok(SpoolStageResult::Rejected(
                        BundleRejection::Oversize,
                        format!(
                            "bundle {bundle_name} grew past MAX_BUNDLE_BYTES ({MAX_BUNDLE_BYTES})"
                        ),
                    ));
                }
                dest.sync_all()
                    .map_err(io_err(&format!("fsync staged {}", staged_dest.display())))?;
                Ok(SpoolStageResult::Staged)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(SpoolStageResult::Missing),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => self
                .inspect_and_stage_spool_bundle_unshare(
                    uid,
                    bundle_name,
                    expected_owner_uid,
                    &spool_dir,
                    &bundle_path,
                    staged_dest,
                ),
            Err(e) => Err(LedgerError::Io {
                context: format!("lstat {}", spool_dir.display()),
                source: e,
            }),
        }
    }

    fn inspect_and_stage_spool_bundle_unshare(
        &self,
        uid: u32,
        bundle_name: &str,
        expected_owner_uid: Option<u32>,
        spool_dir: &Path,
        bundle_path: &Path,
        staged_dest: &Path,
    ) -> Result<SpoolStageResult, LedgerError> {
        let out = Command::new(&self.podman_bin)
            .args([
                "unshare",
                "stat",
                "-c",
                "%F|%a|%u|%s",
                "--",
                &spool_dir.to_string_lossy(),
                &bundle_path.to_string_lossy(),
            ])
            .output()
            .map_err(io_err("podman unshare stat git-spool"))?;

        if !out.status.success() {
            return Ok(SpoolStageResult::Missing);
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.len() != 2 {
            return Ok(SpoolStageResult::Missing);
        }

        let Some((dir_type, dir_mode, _dir_uid, _dir_size)) = parse_stat_line(lines[0]) else {
            return Ok(SpoolStageResult::Missing);
        };
        if dir_type != "directory" {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::ForeignIdentity,
                format!(
                    "git-spool {} is not a directory ({dir_type})",
                    spool_dir.display()
                ),
            ));
        }
        if (dir_mode & 0o022) != 0 {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::ForeignIdentity,
                format!(
                    "git-spool {} has group/world-writable mode {dir_mode:04o}",
                    spool_dir.display()
                ),
            ));
        }

        let Some((file_type, file_mode, file_uid, file_size)) = parse_stat_line(lines[1]) else {
            return Ok(SpoolStageResult::Missing);
        };
        if file_type != "regular file" && file_type != "regular empty file" {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::ForeignIdentity,
                format!(
                    "bundle {} is not a regular file ({file_type})",
                    bundle_path.display()
                ),
            ));
        }
        if (file_mode & 0o022) != 0 {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::ForeignIdentity,
                format!(
                    "bundle {} has group/world-writable mode {file_mode:04o}",
                    bundle_path.display()
                ),
            ));
        }
        let expected_uid = expected_owner_uid.unwrap_or(uid);
        if file_uid != expected_uid {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::ForeignIdentity,
                format!(
                    "bundle {} owner uid {file_uid} does not match expected slot uid {expected_uid}",
                    bundle_path.display()
                ),
            ));
        }
        if file_size > MAX_BUNDLE_BYTES {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::Oversize,
                format!(
                    "bundle {bundle_name} is {file_size} bytes, exceeding MAX_BUNDLE_BYTES ({MAX_BUNDLE_BYTES})"
                ),
            ));
        }

        let cat_out = Command::new(&self.podman_bin)
            .args(["unshare", "cat", "--", &bundle_path.to_string_lossy()])
            .output()
            .map_err(io_err("podman unshare cat bundle"))?;
        if !cat_out.status.success() {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::ForeignIdentity,
                format!("failed to read bundle {} in userns", bundle_path.display()),
            ));
        }
        if (cat_out.stdout.len() as u64) > MAX_BUNDLE_BYTES {
            return Ok(SpoolStageResult::Rejected(
                BundleRejection::Oversize,
                format!("bundle {bundle_name} exceeds MAX_BUNDLE_BYTES ({MAX_BUNDLE_BYTES})"),
            ));
        }
        write_atomic_file(staged_dest, &cat_out.stdout, 0o600)?;
        Ok(SpoolStageResult::Staged)
    }

    fn consume_accepted_spool_bundle(
        &self,
        uid: u32,
        bundle_name: &str,
        ack: &GitBundleAck,
    ) -> Result<(), LedgerError> {
        let spool_dir = self.tree.parcel_git_spool(uid);
        let bundle_path = spool_dir.join(bundle_name);
        let ack_path = spool_dir.join(format!("{bundle_name}.ack"));
        let ack_bytes = ack.render();

        match fs::remove_file(&bundle_path) {
            Ok(()) => {
                write_atomic_file(&ack_path, ack_bytes.as_bytes(), MODE_COMMISSION_DOC)?;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = write_atomic_file(&ack_path, ack_bytes.as_bytes(), MODE_COMMISSION_DOC);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let _ = Command::new(&self.podman_bin)
                    .args(["unshare", "rm", "-f", "--", &bundle_path.to_string_lossy()])
                    .output();
                self.unshare_write_file(&spool_dir, &ack_path, ack_bytes.as_bytes(), 0o644, uid)
            }
            Err(e) => Err(LedgerError::Io {
                context: format!("remove accepted bundle {}", bundle_path.display()),
                source: e,
            }),
        }
    }

    fn quarantine_rejected_spool_bundle(
        &self,
        uid: u32,
        bundle_name: &str,
        rejection: BundleRejection,
        ack: &GitBundleAck,
    ) -> Result<PathBuf, LedgerError> {
        let spool_dir = self.tree.parcel_git_spool(uid);
        let bundle_path = spool_dir.join(bundle_name);
        let rejected_path = spool_dir.join(format!("{bundle_name}.{}", rejection.token()));
        let ack_path = spool_dir.join(format!("{bundle_name}.ack"));
        let ack_bytes = ack.render();

        match fs::rename(&bundle_path, &rejected_path) {
            Ok(()) => {
                write_atomic_file(&ack_path, ack_bytes.as_bytes(), MODE_COMMISSION_DOC)?;
                Ok(rejected_path)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&spool_dir).is_ok() {
                    let _ = write_atomic_file(&ack_path, ack_bytes.as_bytes(), MODE_COMMISSION_DOC);
                }
                Ok(rejected_path)
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let _ = Command::new(&self.podman_bin)
                    .args([
                        "unshare",
                        "mv",
                        "-f",
                        "--",
                        &bundle_path.to_string_lossy(),
                        &rejected_path.to_string_lossy(),
                    ])
                    .output();
                let _ = self.unshare_write_file(
                    &spool_dir,
                    &ack_path,
                    ack_bytes.as_bytes(),
                    0o644,
                    uid,
                );
                Ok(rejected_path)
            }
            Err(e) => Err(LedgerError::Io {
                context: format!(
                    "quarantine rejected bundle {} -> {}",
                    bundle_path.display(),
                    rejected_path.display()
                ),
                source: e,
            }),
        }
    }

    fn unshare_write_file(
        &self,
        dir: &Path,
        target: &Path,
        bytes: &[u8],
        mode: u32,
        owner_uid: u32,
    ) -> Result<(), LedgerError> {
        use std::process::Stdio;
        let script = format!(
            "mkdir -p \"$1\" && cat > \"$2\" && chmod {:04o} \"$2\" && chown {owner_uid}:{owner_uid} \"$2\"",
            mode
        );
        let mut child = Command::new(&self.podman_bin)
            .args([
                "unshare",
                "sh",
                "-c",
                &script,
                "sh",
                &dir.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(io_err("spawn podman unshare write"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(bytes)
                .map_err(io_err("write stdin to podman unshare"))?;
        }
        let out = child
            .wait_with_output()
            .map_err(io_err("wait podman unshare write"))?;
        if !out.status.success() {
            return Err(LedgerError::Io {
                context: format!(
                    "podman unshare write {}: {}",
                    target.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                source: io::Error::other("podman unshare write failed"),
            });
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Query API (tree + charter only)
    // -----------------------------------------------------------------------

    /// List all commissions in ascending ID order, combining `charter.json`
    /// records with `.hamlet/guild/ledger/commission<id>/` tree inspection.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] if the village is not initialized or
    /// `charter.json` cannot be loaded.
    pub fn list_commissions(&self) -> Result<Vec<CommissionSummary>, LedgerError> {
        let charter = self.load_charter_checked()?;
        let mut out = Vec::with_capacity(charter.commissions.len());
        for (&id, record) in &charter.commissions {
            let has_brief = is_regular_file(&self.tree.commission_md(id));
            let has_indenture = is_regular_file(&self.tree.commission_indenture(id));
            let has_bare_git = is_regular_file(&self.tree.commission_git(id).join("HEAD"));
            out.push(CommissionSummary {
                id,
                slug: record.slug.clone(),
                state: record.state(),
                roster: record.roster.clone(),
                has_brief,
                has_indenture,
                has_bare_git,
            });
        }
        Ok(out)
    }

    /// Inspect a single commission by `<slug>`, reading strictly from
    /// `charter.json` and `.hamlet/guild/ledger/commission<id>/`.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError::Sanitize`] if `raw_slug` is invalid,
    /// [`LedgerError::UnknownCommissionSlug`] if no commission has that slug,
    /// or [`LedgerError`] on charter/I/O failures.
    pub fn show_commission(&self, raw_slug: &str) -> Result<CommissionDetail, LedgerError> {
        let slug = sanitize_name(raw_slug)?;
        let charter = self.load_charter_checked()?;
        let Some((&id, record)) = charter.commissions.iter().find(|(_, r)| r.slug == slug) else {
            return Err(LedgerError::UnknownCommissionSlug { slug });
        };

        let roster = record
            .roster
            .iter()
            .map(|&uid| RosterMember {
                uid,
                identity: charter.slots.get(&uid).map(|s| s.identity.clone()),
            })
            .collect();

        let indenture_entries = record
            .indenture_entries
            .iter()
            .map(|(&uid, &status)| IndentureEntryView {
                uid,
                identity: charter.slots.get(&uid).map(|s| s.identity.clone()),
                status,
            })
            .collect();

        let git_dir = self.tree.commission_git(id);
        let git_head = read_head_ref(&git_dir);
        let main_oid = read_ref_oid(&git_dir, DEFAULT_HEAD_REF);

        let md_path = self.tree.commission_md(id);
        let brief = if is_regular_file(&md_path) {
            Some(
                fs::read_to_string(&md_path)
                    .map_err(io_err(&format!("read {}", md_path.display())))?,
            )
        } else {
            None
        };
        let has_indenture = is_regular_file(&self.tree.commission_indenture(id));

        Ok(CommissionDetail {
            id,
            slug: record.slug.clone(),
            state: record.state(),
            roster,
            indenture_entries,
            git_head,
            main_oid,
            brief,
            has_indenture,
        })
    }

    fn load_charter_checked(&self) -> Result<Charter, LedgerError> {
        let charter_path = self.tree.charter();
        match fs::symlink_metadata(&charter_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(LedgerError::SymlinkRefused { path: charter_path });
            }
            Ok(meta) if meta.is_file() => {}
            _ => {
                return Err(LedgerError::UninitializedVillage {
                    root: self.tree.root().to_path_buf(),
                });
            }
        }
        Ok(Charter::load(&charter_path)?)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers: validation, git init, seed import, atomic file writes
// ---------------------------------------------------------------------------

fn validate_brief(brief: &str) -> Result<(), LedgerError> {
    if brief.trim().is_empty() {
        return Err(LedgerError::InvalidBrief {
            reason: "COMMISSION.md brief must not be empty".to_owned(),
        });
    }
    if brief.contains('\r') {
        return Err(LedgerError::InvalidBrief {
            reason: "COMMISSION.md brief must use LF line endings (CR byte rejected)".to_owned(),
        });
    }
    if brief.len() > MAX_ARTIFACT_BYTES {
        return Err(LedgerError::InvalidBrief {
            reason: format!(
                "COMMISSION.md brief is {} bytes, exceeding maximum {}",
                brief.len(),
                MAX_ARTIFACT_BYTES
            ),
        });
    }
    Ok(())
}

/// One validated regular file in `--seed <dir>` ready to be copied into the
/// staging worktree.
#[derive(Debug, Clone)]
struct SeedFile {
    rel: PathBuf,
    src: PathBuf,
    executable: bool,
}

/// Validate `seed_dir` and collect all regular files in deterministic
/// lexicographical order, refusing symlinks or special files anywhere in the
/// tree (ADR-0007).
fn collect_seed_files(seed_dir: &Path) -> Result<Vec<SeedFile>, LedgerError> {
    let meta = fs::symlink_metadata(seed_dir).map_err(|e| LedgerError::SeedDirInvalid {
        path: seed_dir.to_path_buf(),
        reason: e.to_string(),
    })?;
    if meta.file_type().is_symlink() {
        return Err(LedgerError::SymlinkRefused {
            path: seed_dir.to_path_buf(),
        });
    }
    if !meta.is_dir() {
        return Err(LedgerError::SeedDirInvalid {
            path: seed_dir.to_path_buf(),
            reason: "not a directory".to_owned(),
        });
    }

    let mut out = Vec::new();
    walk_seed_dir(seed_dir, seed_dir, &mut out)?;
    Ok(out)
}

fn walk_seed_dir(root: &Path, current: &Path, out: &mut Vec<SeedFile>) -> Result<(), LedgerError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(current)
        .map_err(io_err(&format!("read seed dir {}", current.display())))?
        .map(|entry| {
            entry
                .map(|e| e.path())
                .map_err(io_err(&format!("read entry in {}", current.display())))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();

    for path in entries {
        let meta = fs::symlink_metadata(&path)
            .map_err(io_err(&format!("stat seed entry {}", path.display())))?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            return Err(LedgerError::SymlinkRefused { path });
        }
        // Skip an existing `.git` metadata directory if the user seeds from a
        // git working tree (only after verifying above that `.git` is not a
        // symlink).
        if path.file_name().is_some_and(|n| n == ".git") && ft.is_dir() {
            continue;
        }
        if ft.is_dir() {
            walk_seed_dir(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| LedgerError::SeedDirInvalid {
                    path: path.clone(),
                    reason: "entry escaped seed root".to_owned(),
                })?
                .to_path_buf();
            let executable = (meta.permissions().mode() & 0o111) != 0;
            out.push(SeedFile {
                rel,
                src: path,
                executable,
            });
        } else {
            return Err(LedgerError::SeedDirInvalid {
                path,
                reason: "only regular files and directories are permitted in --seed".to_owned(),
            });
        }
    }
    Ok(())
}

fn create_dir_checked(path: &Path, mode: u32) -> Result<(), LedgerError> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(LedgerError::SymlinkRefused {
                path: path.to_path_buf(),
            });
        }
    }
    fs::create_dir_all(path).map_err(io_err(&format!("create directory {}", path.display())))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_err(&format!(
        "chmod {:04o} {}",
        mode,
        path.display()
    )))?;
    Ok(())
}

fn init_bare_repo(git_dir: &Path) -> Result<(), LedgerError> {
    run_git(
        git_dir,
        None,
        None,
        &["init", "--bare", &git_dir.to_string_lossy()],
        "init --bare",
    )?;
    run_git(
        git_dir,
        None,
        None,
        &["symbolic-ref", "HEAD", DEFAULT_HEAD_REF],
        "symbolic-ref HEAD",
    )?;
    // ADR-0037 cl. 4: no server-side git hooks; the bare repo is passive storage.
    run_git(
        git_dir,
        None,
        None,
        &["config", "core.hooksPath", "/dev/null"],
        "config core.hooksPath",
    )?;
    Ok(())
}

fn import_seed_commit(
    commission_dir: &Path,
    git_dir: &Path,
    commission: u64,
    slug: &SafeName,
    files: &[SeedFile],
) -> Result<String, LedgerError> {
    let worktree = commission_dir.join(".seed-worktree");
    let index_file = commission_dir.join(".seed-index");
    create_dir_checked(&worktree, 0o700)?;

    let res = (|| -> Result<String, LedgerError> {
        for file in files {
            let dest = worktree.join(&file.rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(io_err(&format!("create seed parent {}", parent.display())))?;
            }
            let meta = fs::symlink_metadata(&file.src)
                .map_err(io_err(&format!("stat seed file {}", file.src.display())))?;
            if meta.file_type().is_symlink() {
                return Err(LedgerError::SymlinkRefused {
                    path: file.src.clone(),
                });
            }
            fs::copy(&file.src, &dest).map_err(io_err(&format!(
                "copy seed file {} -> {}",
                file.src.display(),
                dest.display()
            )))?;
            let mode = if file.executable { 0o755 } else { 0o644 };
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode))
                .map_err(io_err(&format!("chmod seed file {}", dest.display())))?;
        }

        run_git(
            git_dir,
            Some(&worktree),
            Some(&index_file),
            &["read-tree", "--empty"],
            "read-tree --empty",
        )?;
        if !files.is_empty() {
            run_git(
                git_dir,
                Some(&worktree),
                Some(&index_file),
                &["add", "--all", "--", "."],
                "add seed files",
            )?;
        }
        let tree_oid = run_git(
            git_dir,
            Some(&worktree),
            Some(&index_file),
            &["write-tree"],
            "write-tree",
        )?;
        let tree_oid = tree_oid.trim();

        let msg = seed_commit_message(commission, slug, files.len(), unix_now());
        let commit_oid = run_git(
            git_dir,
            None,
            None,
            &[
                "-c",
                &format!("user.name={SEED_AUTHOR_NAME}"),
                "-c",
                &format!("user.email={SEED_AUTHOR_EMAIL}"),
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                tree_oid,
                "-m",
                &msg,
            ],
            "commit-tree",
        )?;
        let commit_oid = commit_oid.trim().to_owned();

        run_git(
            git_dir,
            None,
            None,
            &["update-ref", DEFAULT_HEAD_REF, &commit_oid],
            "update-ref refs/heads/main",
        )?;
        Ok(commit_oid)
    })();

    let _ = fs::remove_dir_all(&worktree);
    let _ = fs::remove_file(&index_file);
    res
}

fn run_git(
    git_dir: &Path,
    work_tree: Option<&Path>,
    index_file: Option<&Path>,
    args: &[&str],
    step: &str,
) -> Result<String, LedgerError> {
    let mut cmd = Command::new("git");
    if args.first().copied() != Some("init") {
        cmd.arg(format!("--git-dir={}", git_dir.display()));
    }
    if let Some(wt) = work_tree {
        cmd.arg(format!("--work-tree={}", wt.display()));
    }
    if let Some(idx) = index_file {
        cmd.env("GIT_INDEX_FILE", idx);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args(args);

    let output = cmd.output().map_err(|e| LedgerError::Git {
        step: step.to_owned(),
        detail: e.to_string(),
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        return Err(LedgerError::Git {
            step: step.to_owned(),
            detail,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Ensure every directory under `git_dir` is `0755` (`MODE_SHARED_READONLY`)
/// and every file is world-readable (`mode | 0444`), so rootless container
/// subagents mounting `/var/git/` `:ro` can traverse and fetch regardless of
/// the daemon host user's `umask` (ADR-0011).
fn normalize_repo_permissions(git_dir: &Path) -> Result<(), LedgerError> {
    let meta =
        fs::symlink_metadata(git_dir).map_err(io_err(&format!("stat {}", git_dir.display())))?;
    if meta.file_type().is_symlink() {
        return Err(LedgerError::SymlinkRefused {
            path: git_dir.to_path_buf(),
        });
    }
    if meta.is_dir() {
        fs::set_permissions(git_dir, fs::Permissions::from_mode(MODE_SHARED_READONLY))
            .map_err(io_err(&format!("chmod 0755 {}", git_dir.display())))?;
        for entry in
            fs::read_dir(git_dir).map_err(io_err(&format!("read_dir {}", git_dir.display())))?
        {
            let entry = entry.map_err(io_err(&format!("entry in {}", git_dir.display())))?;
            normalize_repo_permissions(&entry.path())?;
        }
    } else if meta.is_file() {
        let current = meta.permissions().mode() & 0o777;
        let mode = (current | 0o444) & !0o022;
        fs::set_permissions(git_dir, fs::Permissions::from_mode(mode))
            .map_err(io_err(&format!("chmod {:04o} {}", mode, git_dir.display())))?;
    }
    Ok(())
}

fn write_commission_md(path: &Path, brief: &str) -> Result<(), LedgerError> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(LedgerError::TargetExists {
            path: path.to_path_buf(),
        });
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MODE_COMMISSION_DOC)
        .open(path)
        .map_err(io_err(&format!("create {}", path.display())))?;
    file.write_all(brief.as_bytes())
        .map_err(io_err(&format!("write {}", path.display())))?;
    file.sync_all()
        .map_err(io_err(&format!("fsync {}", path.display())))?;
    fs::set_permissions(path, fs::Permissions::from_mode(MODE_COMMISSION_DOC))
        .map_err(io_err(&format!("chmod 0644 {}", path.display())))?;
    Ok(())
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn read_head_ref(git_dir: &Path) -> Option<String> {
    let head_path = git_dir.join("HEAD");
    if !is_regular_file(&head_path) {
        return None;
    }
    let raw = fs::read_to_string(&head_path).ok()?;
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("ref:") {
        Some(rest.trim().to_owned())
    } else if !trimmed.is_empty() {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

fn read_ref_oid(git_dir: &Path, reference: &str) -> Option<String> {
    let loose = git_dir.join(reference);
    if is_regular_file(&loose) {
        if let Ok(raw) = fs::read_to_string(&loose) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    let packed = git_dir.join("packed-refs");
    if is_regular_file(&packed) {
        if let Ok(raw) = fs::read_to_string(&packed) {
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some((oid, r)) = line.split_once(' ') {
                    if r.trim() == reference {
                        return Some(oid.trim().to_owned());
                    }
                }
            }
        }
    }
    None
}

/// Wall-clock unix seconds for lineage commit stamps only (never state,
/// CONVENTIONS §3 r4).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Wall-clock unix milliseconds for escalation lineage stamps only (never
/// state, CONVENTIONS §3 r4).
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}

fn validate_indenture_for_commission(
    raw_bytes: &[u8],
    commission: u64,
    roster: &BTreeSet<u32>,
    charter: &Charter,
) -> Result<BTreeMap<u32, IndentureBlock>, String> {
    let text = std::str::from_utf8(raw_bytes)
        .map_err(|e| format!("INDENTURE.md is not valid UTF-8: {e}"))?;
    let doc = IndentureDoc::parse(text).map_err(|e| e.to_string())?;

    let slot_by_name: BTreeMap<SafeName, u32> = charter
        .slots
        .iter()
        .filter(|(&uid, _)| uid != MAYOR_UID)
        .map(|(&uid, rec)| (rec.identity.clone(), uid))
        .collect();

    let mut resolved = BTreeMap::new();
    let mut errors = Vec::new();
    for (agent_name, block) in &doc.entries {
        match slot_by_name.get(agent_name) {
            None => errors.push(format!(
                "agent '{agent_name}' in INDENTURE.md has no provisioned slot in charter.json \
                 (issue a provision_agent decree for '{agent_name}' first)"
            )),
            Some(&uid) => {
                if !roster.contains(&uid) {
                    errors.push(format!(
                        "agent '{agent_name}' (uid {uid}) in INDENTURE.md is not rostered on \
                         commission{commission}"
                    ));
                } else {
                    resolved.insert(uid, block.clone());
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(resolved)
}

fn read_indenture_ack(path: &Path) -> Result<Option<IndentureAck>, LedgerError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(LedgerError::SymlinkRefused {
                path: path.to_path_buf(),
            });
        }
        Ok(meta) if meta.is_file() => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok(_) | Err(_) => return Ok(None),
    }
    let raw = fs::read_to_string(path).map_err(io_err(&format!("read {}", path.display())))?;
    Ok(IndentureAck::parse(&raw).ok())
}

fn write_indenture_ack(path: &Path, ack: &IndentureAck) -> Result<(), LedgerError> {
    let rendered = ack.render();
    write_atomic_file(path, rendered.as_bytes(), MODE_COMMISSION_DOC)
}

fn quarantine_rejected_indenture(
    indenture_path: &Path,
    rejected_path: &Path,
) -> Result<(), LedgerError> {
    if let Ok(meta) = fs::symlink_metadata(rejected_path) {
        if meta.file_type().is_symlink() {
            return Err(LedgerError::SymlinkRefused {
                path: rejected_path.to_path_buf(),
            });
        }
    }
    fs::rename(indenture_path, rejected_path).map_err(io_err(&format!(
        "quarantine {} -> {}",
        indenture_path.display(),
        rejected_path.display()
    )))
}

fn write_indenture_escalation(
    tree: &HamletTree,
    commission: u64,
    attempt: u32,
    digest: &str,
    error: &str,
) -> Result<PathBuf, LedgerError> {
    let decree_id = DecreeId::new(commission, digest)?;
    let evidence_refs = vec![
        TreeRelPath::parse(&format!(
            "guild/ledger/commission{commission}/COMMISSION.md"
        ))?,
        TreeRelPath::parse(&format!(
            "guild/ledger/commission{commission}/INDENTURE.md.ack"
        ))?,
    ];

    let flat_err = error
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let prefix =
        format!("commission{commission} INDENTURE.md failed validation after {attempt} attempts: ");
    let max_err_chars = hamlet_core::decree::MAX_SUMMARY_LEN
        .saturating_sub(prefix.chars().count())
        .max(16);
    let truncated_err: String = flat_err.chars().take(max_err_chars).collect();
    let summary = format!("{prefix}{truncated_err}");

    let notice = EscalationNotice {
        escalation_version: ESCALATION_VERSION,
        decree_id: decree_id.clone(),
        evidence_refs,
        recorded_unix_ms: unix_now_ms(),
        summary,
    };
    let rendered = notice.render()?;
    let escalations_dir = tree.manor_escalations();
    create_dir_checked(&escalations_dir, MODE_SHARED_READONLY)?;
    let path = tree.manor_escalation_file(&decree_id);
    write_atomic_file(&path, rendered.as_bytes(), MODE_COMMISSION_DOC)?;
    Ok(path)
}

fn append_proprietor_indenture(
    tree: &HamletTree,
    commission: u64,
    uid: u32,
    block: &IndentureBlock,
) -> Result<(), LedgerError> {
    let path = tree.parcel_proprietor(uid);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(LedgerError::SymlinkRefused { path });
        }
        Ok(meta) if meta.is_file() => {}
        _ => return Ok(()),
    }
    let existing =
        fs::read_to_string(&path).map_err(io_err(&format!("read {}", path.display())))?;
    let marker = format!(
        "### Commission {commission} Indenture (`## {}`)",
        block.agent
    );
    if existing.contains(&marker) {
        return Ok(());
    }
    let section = proprietor_indenture_section(commission, uid, block);
    let mut updated = existing;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&section);
    write_atomic_file(&path, updated.as_bytes(), MODE_PARCEL_DOC)
}

fn write_atomic_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), LedgerError> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(LedgerError::SymlinkRefused {
                path: path.to_path_buf(),
            });
        }
    }
    let parent = path.parent().ok_or_else(|| LedgerError::Io {
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
        .map_err(io_err(&format!("create temp {}", tmp_path.display())))?;
    file.write_all(bytes)
        .map_err(io_err(&format!("write temp {}", tmp_path.display())))?;
    file.sync_all()
        .map_err(io_err(&format!("fsync temp {}", tmp_path.display())))?;
    drop(file);

    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode)).map_err(io_err(&format!(
        "chmod {:04o} {}",
        mode,
        tmp_path.display()
    )))?;
    fs::rename(&tmp_path, path).map_err(io_err(&format!(
        "rename {} -> {}",
        tmp_path.display(),
        path.display()
    )))
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `O_NOFOLLOW` for opening staged `.bundle` candidates without following
/// symlinks (ADR-0007, ADR-0037).
#[cfg(target_os = "linux")]
const O_NOFOLLOW_FLAG: i32 = 0o400_000;
#[cfg(not(target_os = "linux"))]
compile_error!("git bundle mediation requires a POSIX O_NOFOLLOW open (ADR-0007)");

enum SpoolStageResult {
    Missing,
    Staged,
    Rejected(BundleRejection, String),
}

fn parse_stat_line(line: &str) -> Option<(&str, u32, u32, u64)> {
    let mut parts = line.trim().splitn(4, '|');
    let file_type = parts.next()?.trim();
    let mode = u32::from_str_radix(parts.next()?.trim(), 8).ok()?;
    let uid = parts.next()?.trim().parse::<u32>().ok()?;
    let size = parts.next()?.trim().parse::<u64>().ok()?;
    Some((file_type, mode, uid, size))
}

fn prepare_quarantine_repo(commission_git: &Path, scratch_repo: &Path) -> Result<(), LedgerError> {
    create_dir_checked(scratch_repo, 0o700)?;
    init_bare_repo(scratch_repo)?;

    let info_dir = scratch_repo.join("objects").join("info");
    create_dir_checked(&info_dir, 0o700)?;
    let alternates = info_dir.join("alternates");
    let objs = commission_git.join("objects");
    write_atomic_file(
        &alternates,
        format!("{}\n", objs.display()).as_bytes(),
        0o600,
    )?;

    let src_refs = commission_git.join("refs");
    let dst_refs = scratch_repo.join("refs");
    if src_refs.is_dir() {
        copy_dir_recursive(&src_refs, &dst_refs)?;
    }

    let src_packed = commission_git.join("packed-refs");
    let dst_packed = scratch_repo.join("packed-refs");
    if is_regular_file(&src_packed) {
        fs::copy(&src_packed, &dst_packed).map_err(io_err(&format!(
            "copy packed-refs {} -> {}",
            src_packed.display(),
            dst_packed.display()
        )))?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), LedgerError> {
    create_dir_checked(dst, 0o700)?;
    for entry in fs::read_dir(src).map_err(io_err(&format!("read_dir {}", src.display())))? {
        let entry = entry.map_err(io_err(&format!("entry in {}", src.display())))?;
        let path = entry.path();
        let meta =
            fs::symlink_metadata(&path).map_err(io_err(&format!("stat {}", path.display())))?;
        if meta.file_type().is_symlink() {
            return Err(LedgerError::SymlinkRefused { path });
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let target = dst.join(name);
        if meta.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if meta.is_file() {
            fs::copy(&path, &target).map_err(io_err(&format!(
                "copy {} -> {}",
                path.display(),
                target.display()
            )))?;
        }
    }
    Ok(())
}
