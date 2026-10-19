//! Typed `.hamlet/` directory layout (SPEC §2 — normative topology).
//!
//! Single source of truth for every village path: `guild/`, `tavern/`,
//! `courier/`, `market/`, `keep/`, `.env`, `charter.json`, `config.json`.
//! All consumers (daemon, cli) must build paths through this module — no
//! ad-hoc `format!`/`PathBuf::push` of village paths (CONVENTIONS §2,
//! ADR-0003: accessor-only path policy).
//!
//! Construction and accessors are pure. The single sanctioned I/O point is
//! [`HamletTree::ensure`], which idempotently creates the full empty tree
//! with correct modes and takes the root as a parameter so tests can run it
//! against tempdirs. Per-UID / per-name locations (mail spools, parcels,
//! patron dirs) are **lazy**: they exist as accessors but are created by the
//! daemon (singular governor) when the identity is allocated, never by
//! `ensure`.
//!
//! Accessors taking externally-sourced names accept only
//! [`crate::sanitize::SafeName`] — traversal is blocked at the API boundary
//! by construction (SPEC §2 name-safety, ADR-0002).

use std::path::{Path, PathBuf};

use crate::decree::DecreeId;
use crate::sanitize::SafeName;

/// Name of the village tree inside a target working directory (SPEC §2).
/// The one place this literal exists; `hamlet init` derives roots through
/// [`HamletTree::in_working_dir`].
pub const VILLAGE_DIR_NAME: &str = ".hamlet";

/// Directory names are private constants so no consumer can misspell them;
/// every literal appears exactly once in this module.
const GUILD: &str = "guild";
const REGISTER: &str = "register";
const BLUEPRINTS: &str = "blueprints";
const LEDGER: &str = "ledger";
const TAVERN: &str = "tavern";
const GOSSIP: &str = "gossip";
const PATRONS: &str = "patrons";
const COURIER: &str = "courier";
const MAIL: &str = "mail";
const QUEUES: &str = "queues";
const FREIGHT: &str = "freight";
const QUARANTINE: &str = ".quarantine";
const DISPATCH: &str = "dispatch";
const OUTBOX: &str = "outbox";
const INBOX: &str = "inbox";
const DEAD: &str = "dead";
const MARKET: &str = "market";
const SKILLS: &str = "skills";
const PKGCACHE: &str = "pkgcache";
const DIST: &str = "dist";
const KEEP: &str = "keep";
const VAULT: &str = "vault";
const ARCHIVES: &str = "archives";
const CENSUS: &str = "census";
const MANOR: &str = "manor";
const HOME: &str = "home";
const DECREES: &str = "decrees";
const REJECTED: &str = ".rejected";
/// The watched human-escalation inbox written from a Mayor
/// `escalate_to_human` decree (SPEC §7; ADR-0029 cl. 4). Outside the
/// Mayor's rw home on purpose: the Mayor may write *only* `decrees/`, so
/// the daemon — the singular governor — is what files the notice where a
/// human (CLI, Sprint 08) will look for it.
const ESCALATIONS: &str = "escalations";

/// Static (identity-independent) directories created by [`HamletTree::ensure`].
/// Per-UID and per-name directories are deliberately absent (lazy creation).
const ENSURE_DIRS: &[&[&str]] = &[
    &[GUILD],
    &[GUILD, REGISTER],
    &[GUILD, BLUEPRINTS],
    &[GUILD, LEDGER],
    &[TAVERN],
    &[TAVERN, GOSSIP],
    &[TAVERN, PATRONS],
    &[COURIER],
    &[COURIER, MAIL],
    &[COURIER, FREIGHT],
    &[COURIER, DISPATCH],
    &[COURIER, DISPATCH, OUTBOX],
    &[COURIER, DISPATCH, INBOX],
    &[COURIER, DISPATCH, DEAD],
    &[MARKET],
    &[MARKET, SKILLS],
    &[MARKET, PKGCACHE],
    &[KEEP],
    &[KEEP, VAULT],
    &[KEEP, ARCHIVES],
    &[KEEP, CENSUS],
    &[KEEP, CENSUS, MANOR],
    &[KEEP, CENSUS, MANOR, HOME],
    &[KEEP, CENSUS, MANOR, HOME, DECREES],
    &[KEEP, CENSUS, MANOR, ESCALATIONS],
];

/// Typed handle on a village root (`.hamlet/`). Cheap to clone; holds only
/// the root path. Every normative location in SPEC §2 has an accessor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HamletTree {
    root: PathBuf,
}

impl HamletTree {
    /// Wrap a root path. Pure — does not touch the filesystem and does not
    /// require the tree to exist. Use [`HamletTree::ensure`] to create it.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Handle for the village inside a *target working directory*
    /// (`<dir>/.hamlet/`, CONVENTIONS §1). Pure; pair with
    /// [`HamletTree::ensure`] to create it.
    #[must_use]
    pub fn in_working_dir(dir: impl AsRef<Path>) -> Self {
        Self::new(dir.as_ref().join(VILLAGE_DIR_NAME))
    }

    /// The village root plus every static (identity-independent) directory
    /// created by [`HamletTree::ensure`], root first, in creation order.
    /// Bootstrap uses this to normalize canonical directory modes; lazy
    /// per-identity dirs are excluded (daemon-owned).
    #[must_use]
    pub fn static_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::with_capacity(ENSURE_DIRS.len() + 1);
        dirs.push(self.root.clone());
        dirs.extend(ENSURE_DIRS.iter().map(|d| self.join(d)));
        dirs
    }

    /// Create the full empty normative tree at `root` (idempotent) and
    /// return its typed handle.
    ///
    /// Modes: directories 0755 (default umask applies); `.env` 0600 —
    /// created empty if absent, and **always** chmod'd to 0600 if present
    /// (CONVENTIONS §6, SPEC §11.4). Per-UID mail spool dirs are lazy and
    /// are NOT created here (see module docs).
    ///
    /// `.env` is classified strictly by `lstat` (ADR-0007): a regular file
    /// is tightened to 0600 with its content preserved; an absent path is
    /// created with `create_new` (`O_CREAT|O_EXCL` — never follows a link
    /// planted mid-call); ANY other file type (symlink, directory, device)
    /// is a typed refusal. `ensure` never opens, creates, or chmods through
    /// a non-regular `.env` (QA F01-02/F01-06).
    ///
    /// This is the single sanctioned I/O point in `hamlet-core` (ADR-0001
    /// purity seam, as amended by ADR-0003 and ADR-0007).
    ///
    /// # Errors
    ///
    /// Any underlying `std::io` failure (permissions, non-directory in the
    /// way, read-only filesystem), or a refusal when `.env` exists but is
    /// not a regular file.
    pub fn ensure(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let tree = Self::new(root);
        for dir in ENSURE_DIRS {
            std::fs::create_dir_all(tree.join(dir))?;
        }
        let env = tree.env_file();
        match std::fs::symlink_metadata(&env) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "{} must be a regular file; refusing to follow or adopt a symlink/dir/other",
                    env.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&env)?;
            }
            Err(e) => return Err(e),
        }
        set_mode_0600(&env)?;
        Ok(tree)
    }

    /// The village root (`.hamlet/` itself).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    // --- top-level files -------------------------------------------------

    /// `.env` — secrets, mode 0600 (SPEC §11.4).
    #[must_use]
    pub fn env_file(&self) -> PathBuf {
        self.join(&[".env"])
    }

    /// `charter.json` — master state map (atomic writes, SPEC §11.1).
    #[must_use]
    pub fn charter(&self) -> PathBuf {
        self.join(&["charter.json"])
    }

    /// `config.json` — orchestrator/engine settings.
    #[must_use]
    pub fn config(&self) -> PathBuf {
        self.join(&["config.json"])
    }

    // --- guild -----------------------------------------------------------

    /// `guild/`
    #[must_use]
    pub fn guild(&self) -> PathBuf {
        self.join(&[GUILD])
    }

    /// `guild/register/` — immutable agent descriptions + allowed scopes.
    #[must_use]
    pub fn guild_register(&self) -> PathBuf {
        self.join(&[GUILD, REGISTER])
    }

    /// `guild/blueprints/` — immutable environment declarations.
    #[must_use]
    pub fn guild_blueprints(&self) -> PathBuf {
        self.join(&[GUILD, BLUEPRINTS])
    }

    /// `guild/blueprints/<name>.toml` — one immutable environment
    /// declaration (SPEC §5). A Mayor `provision_agent` decree names it
    /// with a [`SafeName`](crate::sanitize::SafeName) `blueprint_ref`, so
    /// the file name is built here and nowhere else (CONVENTIONS §4).
    #[must_use]
    pub fn guild_blueprint(&self, name: &crate::sanitize::SafeName) -> PathBuf {
        self.join(&[GUILD, BLUEPRINTS, &format!("{name}.toml")])
    }

    /// `guild/blueprints/<name>.v<version>.toml` — one immutable environment
    /// snapshot version (SPEC §9.4).
    #[must_use]
    pub fn guild_blueprint_version(&self, name: &SafeName, version: u32) -> PathBuf {
        self.join(&[GUILD, BLUEPRINTS, &format!("{name}.v{version}.toml")])
    }

    /// `guild/register/<name>.md` — the immutable agent brief for `name`
    /// (SPEC §5). Provisioning seeds `parcel<uid>/PROPRIETOR.md` from it;
    /// runtime mutations are recorded in the parcel, never here.
    #[must_use]
    pub fn register_brief(&self, name: &SafeName) -> PathBuf {
        self.join(&[GUILD, REGISTER, &format!("{name}.md")])
    }

    /// `guild/ledger/` — all tracked goals.
    #[must_use]
    pub fn guild_ledger(&self) -> PathBuf {
        self.join(&[GUILD, LEDGER])
    }

    /// `guild/ledger/commission<n>/` — one project slice, by index.
    #[must_use]
    pub fn commission(&self, n: u64) -> PathBuf {
        self.join(&[GUILD, LEDGER, &format!("commission{n}")])
    }

    /// `guild/ledger/commission<n>/git/` — central bare git repository.
    #[must_use]
    pub fn commission_git(&self, n: u64) -> PathBuf {
        self.commission(n).join("git")
    }

    /// `guild/ledger/commission<n>/COMMISSION.md` — original player prompt.
    #[must_use]
    pub fn commission_md(&self, n: u64) -> PathBuf {
        self.commission(n).join("COMMISSION.md")
    }

    /// `guild/ledger/commission<n>/INDENTURE.md` — per-agent contracts.
    #[must_use]
    pub fn commission_indenture(&self, n: u64) -> PathBuf {
        self.commission(n).join("INDENTURE.md")
    }

    /// `guild/ledger/commission<n>/INDENTURE.md.ack` — daemon validation ack
    /// for `INDENTURE.md` (SPEC §9.2; ADR-0026 cl. 5).
    #[must_use]
    pub fn commission_indenture_ack(&self, n: u64) -> PathBuf {
        self.commission(n).join("INDENTURE.md.ack")
    }

    // --- tavern ----------------------------------------------------------

    /// `tavern/`
    #[must_use]
    pub fn tavern(&self) -> PathBuf {
        self.join(&[TAVERN])
    }

    /// `tavern/gossip/` — read-only source user input (`/var/context/`).
    #[must_use]
    pub fn tavern_gossip(&self) -> PathBuf {
        self.join(&[TAVERN, GOSSIP])
    }

    /// `tavern/patrons/` — hibernated (unassigned) agent instances.
    #[must_use]
    pub fn tavern_patrons(&self) -> PathBuf {
        self.join(&[TAVERN, PATRONS])
    }

    /// `tavern/patrons/PROPRIETOR<name>/` — one hibernated patron.
    /// Lazy: created by the daemon on hibernation, not by `ensure`.
    #[must_use]
    pub fn patron(&self, name: &SafeName) -> PathBuf {
        self.join(&[TAVERN, PATRONS, &format!("PROPRIETOR{name}")])
    }

    /// `…/PROPRIETOR<name>/home.uid.bak/` — backed-up agent home.
    #[must_use]
    pub fn patron_home_backup(&self, name: &SafeName) -> PathBuf {
        self.patron(name).join("home.uid.bak")
    }

    /// `…/PROPRIETOR<name>/PROPRIETOR.md` — backed-up agent definition.
    #[must_use]
    pub fn patron_md(&self, name: &SafeName) -> PathBuf {
        self.patron(name).join("PROPRIETOR.md")
    }

    // --- courier ---------------------------------------------------------

    /// `courier/`
    #[must_use]
    pub fn courier(&self) -> PathBuf {
        self.join(&[COURIER])
    }

    /// `courier/mail/` — Linux mail spool root (`/var/mail/`). Spool dirs
    /// are per-recipient and lazy (created on first delivery).
    #[must_use]
    pub fn courier_mail(&self) -> PathBuf {
        self.join(&[COURIER, MAIL])
    }

    /// `courier/mail/<recipient>/` — one recipient's spool dir. The
    /// recipient name must already be sanitized; per-UID spools pass the
    /// UID as its decimal [`SafeName`]. Lazy: not created by `ensure`.
    #[must_use]
    pub fn courier_mail_recipient(&self, recipient: &SafeName) -> PathBuf {
        self.join(&[COURIER, MAIL, recipient.as_str()])
    }

    /// `courier/mail/<recipient>/.read/` — the hidden ack dir of one spool
    /// (ADR-0019 cl. 3): delivered/consumed mail is renamed in here and
    /// never rescanned. Hidden, so watcher classification and spool scans
    /// ignore it; daemon-owned (created lazily on first ack).
    #[must_use]
    pub fn courier_mail_read(&self, recipient: &SafeName) -> PathBuf {
        self.join(&[COURIER, MAIL, recipient.as_str(), crate::mail::READ_DIR])
    }

    /// `courier/queues/` — undelivered per-session delivery queues
    /// (daemon-owned spool, file-as-truth for SPEC §6 sequential
    /// delivery; location decision recorded in the Sprint 03 HANDOFF).
    /// Lazy: created on first enqueue, never by `ensure`.
    #[must_use]
    pub fn courier_queues(&self) -> PathBuf {
        self.join(&[COURIER, QUEUES])
    }

    /// `courier/queues/<uid>.jsonl` — the undelivered FIFO queue of one
    /// session slot: one `SessionEnvelope` JSONL record per line. The uid
    /// is a typed `u32` (no externally-sourced name is involved).
    #[must_use]
    pub fn session_queue(&self, uid: u32) -> PathBuf {
        self.join(&[COURIER, QUEUES, &format!("{uid}.jsonl")])
    }

    /// `courier/freight/` — shared folder, POSIX ACL sandboxes.
    #[must_use]
    pub fn courier_freight(&self) -> PathBuf {
        self.join(&[COURIER, FREIGHT])
    }

    /// `courier/freight/<name>/` — one agent-created POSIX ACL sandbox
    /// (SPEC §4 freight ACLs; ADR-0021). The name must already be
    /// sanitized; lazy: created by the agent inside its container, audited
    /// by the daemon on the [`crate::watch::Reaction::FreightDir`] event.
    #[must_use]
    pub fn courier_freight_sandbox(&self, name: &SafeName) -> PathBuf {
        self.join(&[COURIER, FREIGHT, name.as_str()])
    }

    /// `courier/freight/.quarantine/` — daemon-written audit flag dir
    /// (ADR-0021 cl. 5): one JSON flag per violating sandbox, mode 0444,
    /// readable-but-never-writable by agents. Hidden, so watcher
    /// classification and freight scans skip it; lazy: created by the
    /// daemon's freight plane, never by `ensure`.
    #[must_use]
    pub fn courier_freight_quarantine(&self) -> PathBuf {
        self.join(&[COURIER, FREIGHT, QUARANTINE])
    }

    /// `courier/freight/.quarantine/<name>.json` — the quarantine flag of
    /// one sandbox (ADR-0021 cl. 5). Built from a typed [`SafeName`], so
    /// the flag file name is charset-safe by construction.
    #[must_use]
    pub fn courier_freight_quarantine_flag(&self, name: &SafeName) -> PathBuf {
        self.join(&[COURIER, FREIGHT, QUARANTINE, &format!("{}.json", name)])
    }

    /// `courier/dispatch/` — external webhook exchange root.
    #[must_use]
    pub fn courier_dispatch(&self) -> PathBuf {
        self.join(&[COURIER, DISPATCH])
    }

    /// `courier/dispatch/outbox/` — outbound webhook JSON files.
    #[must_use]
    pub fn courier_dispatch_outbox(&self) -> PathBuf {
        self.join(&[COURIER, DISPATCH, OUTBOX])
    }

    /// `courier/dispatch/inbox/` — inbound webhook JSON files.
    #[must_use]
    pub fn courier_dispatch_inbox(&self) -> PathBuf {
        self.join(&[COURIER, DISPATCH, INBOX])
    }

    /// `courier/dispatch/dead/` — dead-lettered outbox requests
    /// (ADR-0020 cl. 5): renamed here after validation failure or retry
    /// exhaustion; operator-recoverable, never daemon-deleted.
    #[must_use]
    pub fn courier_dispatch_dead(&self) -> PathBuf {
        self.join(&[COURIER, DISPATCH, DEAD])
    }

    // --- market ----------------------------------------------------------

    /// `market/`
    #[must_use]
    pub fn market(&self) -> PathBuf {
        self.join(&[MARKET])
    }

    /// `market/skills/` — SKILL.md-defined capabilities.
    #[must_use]
    pub fn market_skills(&self) -> PathBuf {
        self.join(&[MARKET, SKILLS])
    }

    /// `market/pkgcache/` — host-cached package mirrors (`/var/pkgcache/`).
    #[must_use]
    pub fn market_pkgcache(&self) -> PathBuf {
        self.join(&[MARKET, PKGCACHE])
    }

    /// `market/pkgcache/dist/` — the pinned-artifact mirror blueprint
    /// bootstrap fills and reads (ADR-0013 cl. 1); containers see the same
    /// directory at `/var/pkgcache/dist/`.
    ///
    /// Deliberately NOT in [`Self::static_dirs`]: it is a cache subdir of a
    /// normative node, created lazily by the daemon with its parent's 01777
    /// sticky mode so every slot UID can publish into it.
    #[must_use]
    pub fn market_pkgcache_dist(&self) -> PathBuf {
        self.join(&[MARKET, PKGCACHE, DIST])
    }

    /// One mirrored artifact: `market/pkgcache/dist/<file>`.
    ///
    /// `file` is a pin-supplied basename ([`crate::pins::ArtifactPin::file`],
    /// charset-validated there) and is re-checked here — a value containing a
    /// separator, `.`/`..`, or a leading `-` yields `None` instead of a path,
    /// so no caller can turn a cache file name into a traversal or an argv
    /// flag (ADR-0003 typed-paths policy; ADR-0007 never-follow discipline).
    #[must_use]
    pub fn pkgcache_artifact(&self, file: &str) -> Option<PathBuf> {
        if file.is_empty()
            || file.len() > 200
            || file.starts_with('-')
            || file == "."
            || file == ".."
            || file.contains('/')
            || file.contains('\0')
            || file.contains("..")
            || !file.bytes().all(|b| b.is_ascii_graphic())
        {
            return None;
        }
        Some(self.join(&[MARKET, PKGCACHE, DIST, file]))
    }

    // --- keep ------------------------------------------------------------

    /// `keep/`
    #[must_use]
    pub fn keep(&self) -> PathBuf {
        self.join(&[KEEP])
    }

    /// `keep/vault/` — promoted final deliverables.
    #[must_use]
    pub fn keep_vault(&self) -> PathBuf {
        self.join(&[KEEP, VAULT])
    }

    /// `keep/vault/<commission_slug>/` — vault directory for one commission's promoted deliverables.
    #[must_use]
    pub fn keep_vault_commission(&self, commission_slug: &SafeName) -> PathBuf {
        self.join(&[KEEP, VAULT, commission_slug.as_str()])
    }

    /// `keep/vault/<commission_slug>/<artifact>` — one promoted artifact under its commission.
    #[must_use]
    pub fn vault_commission_artifact(
        &self,
        commission_slug: &SafeName,
        artifact: &SafeName,
    ) -> PathBuf {
        self.join(&[KEEP, VAULT, commission_slug.as_str(), artifact.as_str()])
    }

    /// `keep/vault/<artifact>` — one promoted artifact (file or dir).
    /// Lazy: promotion is daemon-performed only (SPEC §11.3).
    #[must_use]
    pub fn vault_artifact(&self, name: &SafeName) -> PathBuf {
        self.join(&[KEEP, VAULT, name.as_str()])
    }

    /// `keep/archives/` — telemetry, log traces, cost metrics.
    #[must_use]
    pub fn keep_archives(&self) -> PathBuf {
        self.join(&[KEEP, ARCHIVES])
    }

    /// `keep/archives/commission<n>/` — commission-specific archives.
    #[must_use]
    pub fn keep_archives_commission(&self, commission: u64) -> PathBuf {
        self.join(&[KEEP, ARCHIVES, &format!("commission{commission}")])
    }

    /// `keep/census/` — active working subtask directories.
    #[must_use]
    pub fn keep_census(&self) -> PathBuf {
        self.join(&[KEEP, CENSUS])
    }

    /// `keep/census/parcel<uid>/` — one agent's lease environment.
    /// Lazy: created on lease, released to `tavern/patrons/` on hibernation.
    #[must_use]
    pub fn parcel(&self, uid: u32) -> PathBuf {
        self.join(&[KEEP, CENSUS, &format!("parcel{uid}")])
    }

    /// `…/parcel<uid>/home/` — persistent agent user home (`/home/<uid>/`).
    #[must_use]
    pub fn parcel_home(&self, uid: u32) -> PathBuf {
        self.parcel(uid).join(HOME)
    }

    /// `…/parcel<uid>/TITLE.md` — chain-of-title lineage.
    #[must_use]
    pub fn parcel_title(&self, uid: u32) -> PathBuf {
        self.parcel(uid).join("TITLE.md")
    }

    /// `…/parcel<uid>/DEED.md` — mutable environment record.
    #[must_use]
    pub fn parcel_deed(&self, uid: u32) -> PathBuf {
        self.parcel(uid).join("DEED.md")
    }

    /// `…/parcel<uid>/PROPRIETOR.md` — mutable agent record.
    #[must_use]
    pub fn parcel_proprietor(&self, uid: u32) -> PathBuf {
        self.parcel(uid).join("PROPRIETOR.md")
    }

    /// `…/parcel<uid>/home/git-spool/` — the push-mediation spool (ADR-0037):
    /// agents stage `.bundle` files here (agent-visible as `/home/<uid>/git-spool/`);
    /// the daemon is the only validator/applier.
    #[must_use]
    pub fn parcel_git_spool(&self, uid: u32) -> PathBuf {
        self.parcel_home(uid).join("git-spool")
    }

    /// `keep/census/archives/git/<commission>/<uid>/` — daemon-owned archive of
    /// accepted git bundles for a commission and agent slot (ADR-0037 cl. 6).
    /// Lazy: created on first accepted bundle, never by `ensure`.
    #[must_use]
    pub fn census_git_archive(&self, commission: u64, uid: u32) -> PathBuf {
        self.join(&[
            KEEP,
            CENSUS,
            ARCHIVES,
            "git",
            &commission.to_string(),
            &uid.to_string(),
        ])
    }

    /// `keep/census/manor/` — the Mayor's lease environment.
    #[must_use]
    pub fn manor(&self) -> PathBuf {
        self.join(&[KEEP, CENSUS, MANOR])
    }

    /// `…/manor/home/` — orchestrator user home (`/home/1000/`).
    #[must_use]
    pub fn manor_home(&self) -> PathBuf {
        self.join(&[KEEP, CENSUS, MANOR, HOME])
    }

    /// `…/manor/MAYOR.md` — trunk system behavior prompt.
    #[must_use]
    pub fn manor_mayor_md(&self) -> PathBuf {
        self.manor().join("MAYOR.md")
    }

    /// `keep/census/manor/home/decrees/` — the watched decree directory
    /// (ADR-0026 cl. 2; `/home/1000/decrees/` in the Mayor container). The
    /// Mayor may write nowhere else (SPEC §4 mounts only its home rw), so
    /// this is the one place it emits decrees. Static: created by
    /// [`HamletTree::ensure`] so a fresh-directory inotify race cannot
    /// swallow the first decree (the Sprint 04 step-02 precedent).
    #[must_use]
    pub fn manor_decrees(&self) -> PathBuf {
        self.join(&[KEEP, CENSUS, MANOR, HOME, DECREES])
    }

    /// `…/manor/home/decrees/<decree_id>.json` — one Mayor-written decree.
    /// The name comes from the typed [`DecreeId`], so it is charset- and
    /// traversal-safe by construction (CONVENTIONS §4).
    #[must_use]
    pub fn manor_decree_file(&self, id: &DecreeId) -> PathBuf {
        self.manor_decrees().join(
            id.decree_file_name()
                .expect("DecreeId renders a SafeName")
                .as_str(),
        )
    }

    /// `…/manor/home/decrees/<decree_id>.ack` — the daemon-written
    /// acknowledgment, next to its decree (ADR-0026 cl. 5).
    #[must_use]
    pub fn manor_decree_ack(&self, id: &DecreeId) -> PathBuf {
        self.manor_decrees().join(
            id.ack_file_name()
                .expect("DecreeId renders a SafeName")
                .as_str(),
        )
    }

    /// `…/manor/home/decrees/.rejected/` — quarantine for decree files that
    /// cannot be parsed into an identity (ADR-0026 cl. 5): daemon-renamed,
    /// hidden (never watched, never rescanned as input), operator-visible.
    #[must_use]
    pub fn manor_decrees_rejected(&self) -> PathBuf {
        self.manor_decrees().join(REJECTED)
    }

    /// `keep/census/manor/escalations/` — the watched escalation inbox
    /// (SPEC §7; ADR-0029 cl. 4): one canonical JSON notice per Mayor
    /// `escalate_to_human` decree. Daemon-written and operator-read; the
    /// Mayor container cannot reach it (SPEC §4 mounts only its home rw),
    /// which is exactly why the daemon writes it. Static for the same
    /// fresh-directory inotify race reason as `decrees/`.
    #[must_use]
    pub fn manor_escalations(&self) -> PathBuf {
        self.join(&[KEEP, CENSUS, MANOR, ESCALATIONS])
    }

    /// `…/manor/escalations/<decree_id>.json` — one escalation notice. The
    /// name is the typed [`DecreeId`], so it is a valid [`SafeName`] and
    /// sorts with the decree it answers (CONVENTIONS §4).
    #[must_use]
    pub fn manor_escalation_file(&self, id: &DecreeId) -> PathBuf {
        self.manor_escalations().join(
            id.decree_file_name()
                .expect("DecreeId renders a SafeName")
                .as_str(),
        )
    }

    /// The one place village paths are composed. Private by design
    /// (ADR-0003): nothing outside this module can concatenate tree paths.
    fn join(&self, parts: &[&str]) -> PathBuf {
        let mut p = self.root.clone();
        for part in parts {
            p.push(part);
        }
        p
    }
}

/// Force mode 0600 on a file, regardless of umask at creation time.
#[cfg(unix)]
fn set_mode_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::sanitize_name;

    fn name(s: &str) -> SafeName {
        sanitize_name(s).expect("test name must sanitize")
    }

    /// Every static normative directory from SPEC §2 exists after `ensure`,
    /// and `ensure` is idempotent.
    #[test]
    fn ensure_creates_full_normative_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".hamlet");
        let tree = HamletTree::ensure(&root).unwrap();
        // second run must not fail or clobber
        std::fs::write(tree.tavern_gossip().join("note.txt"), "hi").unwrap();
        let tree2 = HamletTree::ensure(&root).unwrap();
        assert_eq!(tree, tree2);
        assert!(tree2.tavern_gossip().join("note.txt").exists());

        for dir in [
            tree.guild_register(),
            tree.guild_blueprints(),
            tree.guild_ledger(),
            tree.tavern_gossip(),
            tree.tavern_patrons(),
            tree.courier_mail(),
            tree.courier_freight(),
            tree.courier_dispatch_outbox(),
            tree.courier_dispatch_inbox(),
            tree.courier_dispatch_dead(),
            tree.market_skills(),
            tree.market_pkgcache(),
            tree.keep_vault(),
            tree.keep_archives(),
            tree.keep_census(),
            tree.manor(),
            tree.manor_home(),
        ] {
            assert!(dir.is_dir(), "missing normative dir: {}", dir.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn env_file_is_0600_even_with_loose_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".hamlet");
        let tree = HamletTree::ensure(&root).unwrap();
        let mode = std::fs::metadata(tree.env_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // pre-existing loose .env is tightened, content preserved
        std::fs::set_permissions(tree.env_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(tree.env_file(), "KEY=test-key-do-not-use\n").unwrap();
        HamletTree::ensure(&root).unwrap();
        let mode = std::fs::metadata(tree.env_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(tree.env_file()).unwrap(),
            "KEY=test-key-do-not-use\n"
        );
    }

    /// Identity-scoped locations are accessors only — `ensure` must not
    /// pre-create parcels, patrons, or mail spools (lazy, daemon-owned).
    #[test]
    fn per_identity_locations_are_lazy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".hamlet");
        let tree = HamletTree::ensure(&root).unwrap();
        assert!(!tree.parcel(1001).exists());
        assert!(!tree.patron(&name("mayor")).exists());
        assert!(!tree.courier_mail_recipient(&name("1001")).exists());
        assert!(!tree.vault_artifact(&name("report.md")).exists());
    }

    /// Accessor path shapes match SPEC §2 exactly.
    #[test]
    fn accessor_paths_match_spec() {
        let tree = HamletTree::new(PathBuf::from("/v/.hamlet"));
        assert_eq!(
            HamletTree::in_working_dir(Path::new("/v")),
            tree,
            "in_working_dir must derive /v/.hamlet"
        );
        let n = name("mayor");
        assert_eq!(
            tree.register_brief(&n),
            PathBuf::from("/v/.hamlet/guild/register/mayor.md")
        );
        assert_eq!(
            tree.commission(7),
            PathBuf::from("/v/.hamlet/guild/ledger/commission7")
        );
        assert_eq!(
            tree.commission_git(7),
            PathBuf::from("/v/.hamlet/guild/ledger/commission7/git")
        );
        assert_eq!(
            tree.commission_md(7),
            PathBuf::from("/v/.hamlet/guild/ledger/commission7/COMMISSION.md")
        );
        assert_eq!(
            tree.commission_indenture(7),
            PathBuf::from("/v/.hamlet/guild/ledger/commission7/INDENTURE.md")
        );
        assert_eq!(
            tree.patron(&n),
            PathBuf::from("/v/.hamlet/tavern/patrons/PROPRIETORmayor")
        );
        assert_eq!(
            tree.patron_home_backup(&n),
            PathBuf::from("/v/.hamlet/tavern/patrons/PROPRIETORmayor/home.uid.bak")
        );
        assert_eq!(
            tree.patron_md(&n),
            PathBuf::from("/v/.hamlet/tavern/patrons/PROPRIETORmayor/PROPRIETOR.md")
        );
        assert_eq!(
            tree.courier_mail_recipient(&name("1001")),
            PathBuf::from("/v/.hamlet/courier/mail/1001")
        );
        assert_eq!(
            tree.courier_mail_read(&name("1001")),
            PathBuf::from("/v/.hamlet/courier/mail/1001/.read")
        );
        assert_eq!(
            tree.courier_dispatch_dead(),
            PathBuf::from("/v/.hamlet/courier/dispatch/dead")
        );
        assert_eq!(
            tree.courier_freight_sandbox(&name("exchange")),
            PathBuf::from("/v/.hamlet/courier/freight/exchange")
        );
        assert_eq!(
            tree.courier_freight_quarantine(),
            PathBuf::from("/v/.hamlet/courier/freight/.quarantine")
        );
        assert_eq!(
            tree.courier_freight_quarantine_flag(&name("exchange")),
            PathBuf::from("/v/.hamlet/courier/freight/.quarantine/exchange.json")
        );
        assert_eq!(
            tree.courier_queues(),
            PathBuf::from("/v/.hamlet/courier/queues")
        );
        assert_eq!(
            tree.session_queue(1001),
            PathBuf::from("/v/.hamlet/courier/queues/1001.jsonl")
        );
        assert_eq!(
            tree.vault_artifact(&name("report.md")),
            PathBuf::from("/v/.hamlet/keep/vault/report.md")
        );
        assert_eq!(
            tree.keep_vault_commission(&name("alpha")),
            PathBuf::from("/v/.hamlet/keep/vault/alpha")
        );
        assert_eq!(
            tree.vault_commission_artifact(&name("alpha"), &name("report.md")),
            PathBuf::from("/v/.hamlet/keep/vault/alpha/report.md")
        );
        assert_eq!(
            tree.keep_archives_commission(1),
            PathBuf::from("/v/.hamlet/keep/archives/commission1")
        );
        assert_eq!(
            tree.parcel(1042),
            PathBuf::from("/v/.hamlet/keep/census/parcel1042")
        );
        assert_eq!(
            tree.parcel_home(1042),
            PathBuf::from("/v/.hamlet/keep/census/parcel1042/home")
        );
        assert_eq!(
            tree.parcel_title(1042),
            PathBuf::from("/v/.hamlet/keep/census/parcel1042/TITLE.md")
        );
        assert_eq!(
            tree.parcel_deed(1042),
            PathBuf::from("/v/.hamlet/keep/census/parcel1042/DEED.md")
        );
        assert_eq!(
            tree.parcel_proprietor(1042),
            PathBuf::from("/v/.hamlet/keep/census/parcel1042/PROPRIETOR.md")
        );
        assert_eq!(
            tree.parcel_git_spool(1042),
            PathBuf::from("/v/.hamlet/keep/census/parcel1042/home/git-spool")
        );
        assert_eq!(
            tree.census_git_archive(7, 1042),
            PathBuf::from("/v/.hamlet/keep/census/archives/git/7/1042")
        );
        assert_eq!(
            tree.manor_mayor_md(),
            PathBuf::from("/v/.hamlet/keep/census/manor/MAYOR.md")
        );
        // decree directory layout (ADR-0026 cl. 2)
        assert_eq!(
            tree.manor_decrees(),
            PathBuf::from("/v/.hamlet/keep/census/manor/home/decrees")
        );
        let id = crate::decree::DecreeId::new(7, "c0225c3b8fabf543").unwrap();
        assert_eq!(
            tree.manor_decree_file(&id),
            PathBuf::from(
                "/v/.hamlet/keep/census/manor/home/decrees/d-000000000007-c0225c3b8fabf543.json"
            )
        );
        assert_eq!(
            tree.manor_decree_ack(&id),
            PathBuf::from(
                "/v/.hamlet/keep/census/manor/home/decrees/d-000000000007-c0225c3b8fabf543.ack"
            )
        );
        assert_eq!(
            tree.manor_decrees_rejected(),
            PathBuf::from("/v/.hamlet/keep/census/manor/home/decrees/.rejected")
        );
        assert_eq!(tree.env_file(), PathBuf::from("/v/.hamlet/.env"));
        assert_eq!(tree.charter(), PathBuf::from("/v/.hamlet/charter.json"));
        assert_eq!(tree.config(), PathBuf::from("/v/.hamlet/config.json"));
    }

    /// `static_dirs` covers the root + exactly the dirs `ensure` creates,
    /// and nothing lazy.
    #[test]
    fn static_dirs_match_ensure_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".hamlet");
        let tree = HamletTree::ensure(&root).unwrap();
        let statics = tree.static_dirs();
        assert_eq!(statics[0], root);
        assert!(statics.iter().all(|d| d.is_dir()));
        // every static dir is inside the root; lazy dirs are not listed
        assert!(statics.iter().all(|d| d.starts_with(&root)));
        assert!(!statics.contains(&tree.parcel(1001)));
        assert!(!statics.contains(&tree.commission(1)));
        // count: root + ENSURE_DIRS
        assert_eq!(statics.len(), 26);
    }

    /// Every accessor stays inside the root — a SafeName can never escape.
    #[test]
    fn accessors_never_escape_root() {
        let tree = HamletTree::new(PathBuf::from("/v/.hamlet"));
        let hostile_but_safe = name("a-b_c.d");
        for p in [
            tree.patron(&hostile_but_safe),
            tree.vault_artifact(&hostile_but_safe),
            tree.courier_mail_recipient(&hostile_but_safe),
            tree.register_brief(&hostile_but_safe),
        ] {
            assert!(p.starts_with(tree.root()));
        }
    }
    /// `market/pkgcache/dist/` mirror accessors (ADR-0013): lazy (not a
    /// static dir), inside the root, and hostile file names yield `None`
    /// instead of a path.
    #[test]
    fn pkgcache_mirror_accessors_are_typed_and_traversal_proof() {
        let tree = HamletTree::new(PathBuf::from("/v/.hamlet"));
        let dist = tree.market_pkgcache_dist();
        assert_eq!(dist, PathBuf::from("/v/.hamlet/market/pkgcache/dist"));
        assert!(
            !tree.static_dirs().contains(&dist),
            "a cache subdir of a normative node is created lazily"
        );
        assert_eq!(
            tree.pkgcache_artifact("pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz"),
            Some(dist.join("pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz"))
        );
        for hostile in [
            "",
            ".",
            "..",
            "../evil.tar.gz",
            "a/b",
            "/etc/passwd",
            "-rf",
            "a..b",
            "a b",
            "\u{e9}vil",
            &"x".repeat(201),
        ] {
            assert_eq!(tree.pkgcache_artifact(hostile), None, "{hostile:?}");
        }
    }
}
