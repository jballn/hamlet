//! Mail routing & delivery (SPEC §8.1; format from `hamlet_core::mail`,
//! lifecycle ADR-0019 cl. 2–4).
//!
//! Watches `courier/mail/` spools (via [`crate::watch`] reactions) and
//! routes messages sequentially into the recipient's Sprint 03 delivery
//! queue ([`Supervisor::enqueue`]) — one task boundary at a time, never
//! interleaved mid-message (SPEC §6). The router is the *only* consumer of
//! spool files: every route ends in exactly one of
//!
//! - **delivered** — envelope persisted to `courier/queues/<uid>.jsonl`,
//!   then the spool file is ack-renamed into the recipient's hidden
//!   `.read/` dir (enqueue-before-ack, ADR-0019 cl. 3: a crash between
//!   the two re-delivers at most once from the spool, never drops);
//! - **bounced** — the original is consumed into `.read/` and a
//!   `From: system` bounce mail is written into the *sender's* spool
//!   (typed [`BounceReason`], ADR-0019 cl. 4) plus a sled trace;
//! - **already consumed** — the file vanished before routing (a previous
//!   run acked it): idempotent no-op, the restart-recovery guarantee.
//!
//! # Restart recovery (no duplicates, provable from the tree)
//!
//! [`MailRouter::recover_spools`] scans spool **top levels only** —
//! `.read/` is never rescanned — and routes every pending `*.mail` in
//! sorted order. [`spawn_mail_plane`] runs it as the watcher's startup
//! hook, *inside the watcher task before any event dispatch*, so recovery
//! and event-driven routing can never race on the same file. Acked mail
//! (in `.read/`) is invisible to recovery: zero double-delivery.
//!
//! # Sending (daemon-asserted identity)
//!
//! [`MailRouter::send`] is the only write path into a spool for in-daemon
//! actors (Mayor, agents, CLI/patrons, system notices). The [`MailActor`]
//! *is* the `From:` identity — the API has no sender parameter, so a
//! caller can never claim an identity the daemon did not assert. Writes
//! are atomic: `.`-prefixed temp in the target spool dir → fsync →
//! rename → dir fsync (ADR-0006 discipline); the watcher then routes the
//! bounce/send like any other incoming mail (one delivery path).
//!
//! # Ordering & storms
//!
//! Per-writer FIFO falls out of the spool name grammar
//! (`{seq:012}-…`, lexicographic scan order): reactions are *hints*, and
//! each hint triggers a **sorted scan of the whole recipient spool**
//! ([`MailRouter::route_recipient`]) in the *single sequential* handler
//! task — so ordering never depends on event arrival order, and lost
//! events self-heal on the next hint or restart. [`MailRouter::send`]
//! pushes its own hint straight into the handler (in-daemon sends never
//! depend on inotify timing — a file written into a *freshly created*
//! spool dir can precede the watch attachment), and
//! [`MailRouter::ensure_spool_dirs`] pre-creates a spool dir for every
//! charter slot at startup so external writers never race the watch
//! either. 1k rapid spool writes all land, ordered per recipient, through
//! bounded channels (bounded memory — backpressure instead of heap
//! growth).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::charter::{Charter, CharterError};
use hamlet_core::mail::{
    is_incoming, is_slot_uid, parse_spool_file_name, safe_spool_name, spool_file_name,
    BounceReason, Mail, MailHeader, MailSender, SYSTEM_SENDER,
};
use hamlet_core::sanitize::SafeName;
use hamlet_core::session::{CorrelationId, QueuedKind, SessionStatus};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use tokio::sync::mpsc;

use crate::delivery::QueueError;
use crate::supervise::{SuperviseError, Supervisor};
use crate::telemetry::{AgentKey, TelemetryError, TraceRecord};
use crate::watch::{spawn_tree_watcher, Dispatcher, WatchError, WatcherHandle};

/// Typed mail-routing failures. Bounces are *outcomes*
/// ([`RouteOutcome::Bounced`]), not errors: an error means the spool file
/// stays untouched and a later event or restart recovery retries it.
#[derive(Debug)]
pub enum MailRouteError {
    /// Filesystem failure with context.
    Io {
        /// What was attempted.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
    /// Charter load/save failure during the cross-check.
    Charter(CharterError),
    /// Session/queue failure that is *not* overflow (persistence,
    /// corruption) — the mail stays in the spool for retry.
    Supervise(SuperviseError),
    /// Telemetry counter failure (spool seq / correlation seq).
    Telemetry(TelemetryError),
    /// An outgoing daemon-composed mail failed format validation.
    Mail(hamlet_core::mail::MailError),
    /// Watcher startup failure ([`spawn_mail_plane`]).
    Watch(WatchError),
}

impl fmt::Display for MailRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Charter(e) => write!(f, "charter: {e}"),
            Self::Supervise(e) => write!(f, "supervise: {e}"),
            Self::Telemetry(e) => write!(f, "telemetry: {e}"),
            Self::Mail(e) => write!(f, "mail format: {e}"),
            Self::Watch(e) => write!(f, "watcher: {e}"),
        }
    }
}

impl std::error::Error for MailRouteError {}

impl From<CharterError> for MailRouteError {
    fn from(e: CharterError) -> Self {
        Self::Charter(e)
    }
}

impl From<SuperviseError> for MailRouteError {
    fn from(e: SuperviseError) -> Self {
        Self::Supervise(e)
    }
}

impl From<TelemetryError> for MailRouteError {
    fn from(e: TelemetryError) -> Self {
        Self::Telemetry(e)
    }
}

impl From<hamlet_core::mail::MailError> for MailRouteError {
    fn from(e: hamlet_core::mail::MailError) -> Self {
        Self::Mail(e)
    }
}

impl From<WatchError> for MailRouteError {
    fn from(e: WatchError) -> Self {
        Self::Watch(e)
    }
}

/// The in-daemon actor a sent mail originates from. This enum — never a
/// caller-supplied header — is the `From:` identity (daemon-asserted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailActor {
    /// The Mayor (slot UID 1000).
    Mayor,
    /// A supervised agent session (slot UID 1001–2000); the daemon only
    /// ever passes the uid of the session it is acting for.
    Agent(u32),
    /// Daemon system notices and bounces (`From: system`).
    System,
    /// A human patron routed through the CLI/web surface (sanitized name).
    Patron(SafeName),
}

impl MailActor {
    /// The asserted `From:` identity.
    #[must_use]
    pub fn sender(&self) -> MailSender {
        match self {
            Self::Mayor => MailSender::Uid(MAYOR_UID),
            Self::Agent(uid) => MailSender::Uid(*uid),
            Self::System => MailSender::Name(system_name()),
            Self::Patron(name) => MailSender::Name(name.clone()),
        }
    }

    /// Slot uid for the spool seq counter and conforming file naming;
    /// `None` for non-slot actors (foreign spool names — still incoming).
    fn writer_uid(&self) -> Option<u32> {
        match self {
            Self::Mayor => Some(MAYOR_UID),
            Self::Agent(uid) => Some(*uid),
            Self::System | Self::Patron(_) => None,
        }
    }

    /// Label segment for foreign spool file names (`{seq:012}-<label>-…`).
    fn label(&self) -> String {
        match self {
            Self::Mayor => MAYOR_UID.to_string(),
            Self::Agent(uid) => uid.to_string(),
            Self::System => SYSTEM_SENDER.to_owned(),
            Self::Patron(name) => name.as_str().to_owned(),
        }
    }
}

/// The Mayor-mail hook seam (SPEC §7, §8.1).
///
/// Mail addressed to UID 1000 is not queued for a stdin the Mayor does not
/// have: the Mayor is woken with the message as a packet artifact
/// ([`crate::mayor::WakeEvent::from_mail_spool`], ADR-0028 cl. 1 — one
/// process per wake). The router owns the *routing* decision (charter
/// cross-checks, bounce rules) and hands the untouched spool path to this
/// hook, which is where the wake happens — before the ack-rename, so the
/// artifact is always loaded from a file that still exists.
pub trait MayorMail: Send + Sync {
    /// Hand one routed spool file (path + recipient slot) to the Mayor.
    fn mayor_mail(&self, spool: &Path);
}

/// What one spool-file route ended in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Enqueued for sequential stdin delivery, spool file ack-renamed into
    /// `.read/`.
    Delivered {
        /// Recipient slot.
        uid: u32,
        /// Queue correlation id (ties to sled traces / stream events).
        correlation: CorrelationId,
    },
    /// Undeliverable: original consumed into `.read/`; a `From: system`
    /// bounce was written to `to` (or dropped with a trace when the sender
    /// is not a routable slot uid — e.g. `system` itself, which bounds
    /// bounce chains at depth 2).
    Bounced {
        /// Why (ADR-0019 cl. 4).
        reason: BounceReason,
        /// Bounce recipient, when one was routable.
        to: Option<u32>,
    },
    /// Delivered to the Mayor as a wake instead of the delivery queue: the
    /// spool file was ack-renamed and the Mayor carries the message inside
    /// its next single-turn context packet (SPEC §7).
    WokenMayor {
        /// The Mayor's slot (always [`MAYOR_UID`]).
        uid: u32,
    },
    /// The spool file was already gone (acked by a previous run or a
    /// concurrent consumer) — idempotent no-op.
    AlreadyConsumed,
}

/// Summary of one recipient spool scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpoolReport {
    /// Incoming spool files seen.
    pub scanned: usize,
    /// Routes that ended [`RouteOutcome::Delivered`].
    pub delivered: usize,
    /// Routes that ended [`RouteOutcome::Bounced`].
    pub bounced: usize,
}

/// Summary of one startup recovery scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Incoming spool files seen.
    pub scanned: usize,
    /// Routes that ended [`RouteOutcome::Delivered`].
    pub delivered: usize,
    /// Routes that ended [`RouteOutcome::Bounced`].
    pub bounced: usize,
}

/// The village mail router: spool consumer + daemon-side sender. All
/// methods take `&self`; the daemon holds one instance per village (the
/// singular governor, CONVENTIONS §2) and its handler task is the only
/// sequential consumer.
pub struct MailRouter {
    tree: HamletTree,
    supervisor: Arc<Supervisor>,
    /// Temp-file serial for atomic spool writes (unique `.`-prefixed temp
    /// names within this process; `&self` interior mutability).
    tmp_serial: AtomicU64,
    /// Direct recipient-rescan hints to the handler task ([`Self::send`]
    /// pushes here; the watcher reaction stream stays the external path).
    hints: Mutex<Option<mpsc::UnboundedSender<SafeName>>>,
    /// Optional Mayor-mail hook (SPEC §7): installed by the Mayor plane.
    /// `None` means no Mayor is wired, and UID-1000 mail keeps the normal
    /// queue path so it is never lost.
    mayor_mail: Mutex<Option<Arc<dyn MayorMail>>>,
}

impl MailRouter {
    /// Router over the supervisor's village tree (queues, charter access
    /// and telemetry come from the supervisor — one governor seam).
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self {
            tree: supervisor.tree().clone(),
            supervisor,
            tmp_serial: AtomicU64::new(0),
            hints: Mutex::new(None),
            mayor_mail: Mutex::new(None),
        }
    }

    /// Install the Mayor-mail hook (SPEC §7). The Mayor plane calls this at
    /// wiring time; without it, mail to UID 1000 falls back to the delivery
    /// queue (never dropped, just not a wake).
    pub fn with_mayor_mail(mut self, hook: Arc<dyn MayorMail>) -> Self {
        self.mayor_mail = Mutex::new(Some(hook));
        self
    }

    /// The village tree this router reads/writes.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    // --- routing -----------------------------------------------------------

    /// Route one incoming spool file end-to-end (parse → cross-check →
    /// enqueue → ack, or consume → bounce). Idempotent: a file that is
    /// already gone yields [`RouteOutcome::AlreadyConsumed`].
    ///
    /// # Errors
    ///
    /// [`MailRouteError`] on I/O, charter, queue-persistence or telemetry
    /// failures — the spool file then stays pending for the next event or
    /// restart recovery (at-least-once from the tree, never dropped).
    pub fn route(
        &self,
        recipient: &SafeName,
        file: &SafeName,
    ) -> Result<RouteOutcome, MailRouteError> {
        let spool = self
            .tree
            .courier_mail_recipient(recipient)
            .join(file.as_str());
        let raw = match fs::read(&spool) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(RouteOutcome::AlreadyConsumed)
            }
            Err(source) => {
                return Err(MailRouteError::Io {
                    context: format!("read {}", spool.display()),
                    source,
                })
            }
        };
        let text = String::from_utf8_lossy(&raw);
        let owner = recipient
            .as_str()
            .parse::<u32>()
            .ok()
            .filter(|uid| is_slot_uid(*uid));

        match Mail::parse(&text) {
            Ok(mail) => {
                let Some(uid) = owner else {
                    let parsed = recipient.as_str().parse::<u32>().unwrap_or(0);
                    return self.bounce(
                        recipient,
                        file,
                        &BounceReason::NoSlot { uid: parsed },
                        Some(&mail.header),
                        mail_sender_uid(&mail),
                    );
                };
                if mail.header.to != uid {
                    return self.bounce(
                        recipient,
                        file,
                        &BounceReason::Malformed {
                            detail: format!(
                                "To {} does not match spool owner {uid}",
                                mail.header.to
                            ),
                        },
                        Some(&mail.header),
                        mail_sender_uid(&mail),
                    );
                }
                self.deliver(recipient, file, uid, &mail)
            }
            Err(err) => {
                // Malformed: the writer uid from a conforming spool name is
                // the only trustworthy bounce target (headers are unusable).
                let writer = parse_spool_file_name(file.as_str()).map(|spool_ref| spool_ref.writer);
                self.bounce(
                    recipient,
                    file,
                    &BounceReason::Malformed {
                        detail: err.to_string(),
                    },
                    None,
                    writer,
                )
            }
        }
    }

    /// Charter cross-check → enqueue → ack (the happy path).
    fn deliver(
        &self,
        recipient: &SafeName,
        file: &SafeName,
        uid: u32,
        mail: &Mail,
    ) -> Result<RouteOutcome, MailRouteError> {
        let charter = Charter::load(&self.tree.charter())?;
        let agent = AgentKey {
            commission: charter
                .slots
                .get(&uid)
                .and_then(|slot| slot.commission)
                .unwrap_or(0),
            uid,
        };
        if !charter.slots.contains_key(&uid) {
            return self.bounce(
                recipient,
                file,
                &BounceReason::NoSlot { uid },
                Some(&mail.header),
                mail_sender_uid(mail),
            );
        }
        if let Some(record) = charter.sessions.get(&uid) {
            if matches!(
                record.status,
                SessionStatus::Crashed | SessionStatus::Hibernating
            ) {
                return self.bounce(
                    recipient,
                    file,
                    &BounceReason::SessionUnavailable {
                        uid,
                        state: record.status.to_string(),
                    },
                    Some(&mail.header),
                    mail_sender_uid(mail),
                );
            }
        }
        // The Mayor has no stdin queue: its single-turn packet *is* the
        // delivery channel (SPEC §7, ADR-0028 cl. 1). Hand the untouched
        // spool path to the wake hook before the ack-rename.
        let mayor_hook = self
            .mayor_mail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if uid == MAYOR_UID {
            if let Some(hook) = mayor_hook {
                let spool = self
                    .tree
                    .courier_mail_recipient(recipient)
                    .join(file.as_str());
                hook.mayor_mail(&spool);
                self.ack(recipient, file)?;
                self.trace(
                    agent,
                    "mail-woken-mayor",
                    format!("{} → uid {MAYOR_UID}", file.as_str()),
                );
                return Ok(RouteOutcome::WokenMayor { uid });
            }
        }
        let correlation = match self
            .supervisor
            .enqueue(uid, QueuedKind::Mail, &mail.render())
        {
            Ok(correlation) => correlation,
            Err(SuperviseError::Queue(QueueError::Overflow { depth, .. })) => {
                return self.bounce(
                    recipient,
                    file,
                    &BounceReason::QueueOverflow {
                        uid,
                        depth: u32::try_from(depth).unwrap_or(u32::MAX),
                    },
                    Some(&mail.header),
                    mail_sender_uid(mail),
                );
            }
            Err(SuperviseError::NoSlot(uid)) => {
                return self.bounce(
                    recipient,
                    file,
                    &BounceReason::NoSlot { uid },
                    Some(&mail.header),
                    mail_sender_uid(mail),
                );
            }
            // Persistence/corruption failures: leave the file pending —
            // the next event or restart recovery retries (never dropped).
            Err(e) => return Err(e.into()),
        };
        // Enqueue-before-ack (ADR-0019 cl. 3): the envelope is fsync'd in
        // courier/queues/ before the spool file is consumed.
        self.ack(recipient, file)?;
        self.trace(
            agent,
            "mail-routed",
            format!("{} → uid {uid} as {correlation}", file.as_str()),
        );
        Ok(RouteOutcome::Delivered { uid, correlation })
    }

    /// Consume the original into `.read/`, write the `From: system` bounce
    /// to the sender (when routable), trace, and report the outcome.
    fn bounce(
        &self,
        recipient: &SafeName,
        file: &SafeName,
        reason: &BounceReason,
        original: Option<&MailHeader>,
        sender_uid: Option<u32>,
    ) -> Result<RouteOutcome, MailRouteError> {
        self.ack(recipient, file)?;
        // Trace under the sender's real agent key (commission from the
        // charter, best effort — telemetry loss degrades, never fails).
        let commission = sender_uid
            .and_then(|uid| {
                Charter::load(&self.tree.charter())
                    .ok()
                    .and_then(|charter| charter.slots.get(&uid).and_then(|s| s.commission))
            })
            .unwrap_or(0);
        let agent = AgentKey {
            commission,
            uid: sender_uid.unwrap_or(0),
        };
        let mut to = None;
        if let Some(sender) = sender_uid {
            let body = bounce_body(reason, original);
            let subject = BounceReason::bounce_subject(original.map(|h| h.subject.as_str()));
            match self.send(MailActor::System, sender, &subject, &body, None) {
                Ok(_) => to = Some(sender),
                Err(e) => self.trace(
                    agent,
                    "mail-bounce-send-failed",
                    format!("{}: {e}", file.as_str()),
                ),
            }
        }
        self.trace(
            agent,
            "mail-bounced",
            format!("{}: {reason}", file.as_str()),
        );
        Ok(RouteOutcome::Bounced {
            reason: reason.clone(),
            to,
        })
    }

    /// Ack-rename a consumed spool file into the recipient's hidden
    /// `.read/` dir; collisions get the `.r<n>` suffix before the
    /// extension (ADR-0019 cl. 3). A vanished source is a no-op.
    fn ack(&self, recipient: &SafeName, file: &SafeName) -> Result<(), MailRouteError> {
        let read_dir = self.tree.courier_mail_read(recipient);
        fs::create_dir_all(&read_dir).map_err(|source| MailRouteError::Io {
            context: format!("create {}", read_dir.display()),
            source,
        })?;
        let source = self
            .tree
            .courier_mail_recipient(recipient)
            .join(file.as_str());
        for attempt in 0u32.. {
            let name = ack_file_name(file.as_str(), attempt);
            let target = read_dir.join(&name);
            if target.exists() {
                continue;
            }
            match fs::rename(&source, &target) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // consumed elsewhere
                Err(e) => {
                    return Err(MailRouteError::Io {
                        context: format!("ack {} → {}", source.display(), target.display()),
                        source: e,
                    })
                }
            }
        }
        unreachable!("ack collision suffix space is unbounded")
    }

    // --- sending (daemon-asserted identity) ---------------------------------

    /// Atomically write a mail into the target's spool. `From:` is the
    /// [`MailActor`] — daemon-asserted, never caller-claimed. The spool
    /// seq comes from the durable telemetry counter (monotonic across
    /// restarts, no wall-clock input — CONVENTIONS §3.4); `date` is an
    /// opaque lineage token (default: unix-seconds stamp).
    ///
    /// Returns the final spool path. The watcher routes it like any other
    /// incoming mail — sending never delivers directly (one code path).
    ///
    /// # Errors
    ///
    /// [`MailRouteError::Mail`] when the composed message fails format
    /// validation (bad subject/body/recipient), telemetry or I/O failures.
    pub fn send(
        &self,
        actor: MailActor,
        to: u32,
        subject: &str,
        body: &str,
        date: Option<&str>,
    ) -> Result<PathBuf, MailRouteError> {
        let mail = Mail {
            header: MailHeader {
                from: actor.sender(),
                to,
                subject: subject.to_owned(),
                date: date.map_or_else(default_date_token, ToOwned::to_owned),
            },
            body: body.to_owned(),
        };
        // Validate through the same parser every incoming mail passes —
        // the daemon never writes a spool file it would bounce.
        let rendered = mail.render();
        Mail::parse(&rendered)?;
        let recipient = safe_spool_name(&to.to_string()).map_err(|_| {
            MailRouteError::Mail(hamlet_core::mail::MailError::InvalidTo {
                value: to.to_string(),
            })
        })?;
        let seq = self
            .supervisor
            .telemetry()
            .next_spool_seq(actor.writer_uid().unwrap_or(0))?;
        let nonce = format!("{seq:x}");
        let file_name = match actor.writer_uid() {
            Some(uid) => spool_file_name(seq, uid, &nonce)?,
            None => format!("{seq:012}-{}-{nonce}.mail", actor.label()),
        };
        let dir = self.tree.courier_mail_recipient(&recipient);
        fs::create_dir_all(&dir).map_err(|source| MailRouteError::Io {
            context: format!("create {}", dir.display()),
            source,
        })?;
        // Atomic writer visibility (ADR-0019 cl. 2): hidden temp + rename.
        let serial = self.tmp_serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{}-{serial}.mail", std::process::id()));
        let final_path = dir.join(&file_name);
        let result = (|| -> io::Result<()> {
            {
                let mut out = fs::File::create(&tmp)?;
                std::io::Write::write_all(&mut out, rendered.as_bytes())?;
                out.sync_all()?;
            }
            fs::rename(&tmp, &final_path)?;
            if let Ok(dir_handle) = fs::File::open(&dir) {
                let _ = dir_handle.sync_all();
            }
            Ok(())
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&tmp);
            return Err(MailRouteError::Io {
                context: format!("write {}", final_path.display()),
                source,
            });
        }
        self.trace(
            AgentKey {
                commission: 0,
                uid: actor.writer_uid().unwrap_or(0),
            },
            "mail-sent",
            format!("{} → {to} as {file_name}", actor.label()),
        );
        // Direct hint to the handler task: delivery of in-daemon sends
        // never depends on inotify timing (fresh-dir watch races). Without
        // an attached handler the file simply waits for the next scan or
        // restart recovery (file-as-truth).
        if let Some(tx) = self
            .hints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(recipient);
        }
        Ok(final_path)
    }

    /// Pre-create the spool dir of every charter slot (idempotent) so the
    /// watcher holds an inotify watch on each spool **before** external
    /// writers can create files inside — the fresh-directory event race
    /// cannot occur for provisioned slots. Run as part of
    /// [`spawn_mail_plane`]'s startup hook; provisioning that allocates
    /// new slots at runtime should re-run it.
    ///
    /// # Errors
    ///
    /// Charter load or directory-creation failures.
    pub fn ensure_spool_dirs(&self) -> Result<usize, MailRouteError> {
        let charter = Charter::load(&self.tree.charter())?;
        let mut ensured = 0;
        for uid in charter.slots.keys() {
            let recipient = safe_spool_name(&uid.to_string()).map_err(|_| {
                MailRouteError::Mail(hamlet_core::mail::MailError::InvalidTo {
                    value: uid.to_string(),
                })
            })?;
            let dir = self.tree.courier_mail_recipient(&recipient);
            if !dir.exists() {
                fs::create_dir_all(&dir).map_err(|source| MailRouteError::Io {
                    context: format!("create {}", dir.display()),
                    source,
                })?;
                ensured += 1;
            }
        }
        Ok(ensured)
    }

    // --- restart recovery ----------------------------------------------------

    /// Route **every** pending incoming file in one recipient's spool,
    /// sorted (per-writer FIFO by the name grammar). Reactions are treated
    /// as *hints*: scanning the whole spool makes routing independent of
    /// event delivery quirks (a lost event on a freshly-created spool dir,
    /// kernel queue overflow) and idempotent re-scans are cheap
    /// ([`RouteOutcome::AlreadyConsumed`]). The single sequential handler
    /// task is the only caller at runtime, so two scans never race.
    ///
    /// # Errors
    ///
    /// Directory-scan I/O failures, or the first route error encountered
    /// (that file stays pending for the next scan or restart recovery).
    pub fn route_recipient(&self, recipient: &SafeName) -> Result<SpoolReport, MailRouteError> {
        let mut report = SpoolReport::default();
        let dir = self.tree.courier_mail_recipient(recipient);
        for file in sorted_dir_names(&dir)? {
            if !is_incoming(&file) {
                continue;
            }
            let Ok(file) = safe_spool_name(&file) else {
                continue; // foreign name with unsafe chars: not routable
            };
            report.scanned += 1;
            match self.route(recipient, &file)? {
                RouteOutcome::Delivered { .. } | RouteOutcome::WokenMayor { .. } => {
                    report.delivered += 1;
                }
                RouteOutcome::Bounced { .. } => report.bounced += 1,
                RouteOutcome::AlreadyConsumed => {}
            }
        }
        Ok(report)
    }

    /// Scan every recipient spool **top level** (never `.read/`) and route
    /// all pending incoming mail in sorted order — the daemon-restart
    /// recovery path (ADR-0019 cl. 3: acked mail is invisible, so this
    /// cannot duplicate an already-delivered message).
    ///
    /// # Errors
    ///
    /// Directory-scan I/O failures, or the first route error encountered
    /// (that file stays pending for the next recovery).
    pub fn recover_spools(&self) -> Result<RecoveryReport, MailRouteError> {
        let mut report = RecoveryReport::default();
        for recipient in sorted_dir_names(&self.tree.courier_mail())? {
            if recipient.starts_with('.') {
                continue; // .read siblings, temp dirs — never spool owners
            }
            let Ok(recipient) = safe_spool_name(&recipient) else {
                continue; // foreign dir name: not a routable spool owner
            };
            let scanned = self.route_recipient(&recipient)?;
            report.scanned += scanned.scanned;
            report.delivered += scanned.delivered;
            report.bounced += scanned.bounced;
        }
        self.trace(
            AgentKey {
                commission: 0,
                uid: 0,
            },
            "mail-recovered",
            format!(
                "scanned={} delivered={} bounced={}",
                report.scanned, report.delivered, report.bounced
            ),
        );
        Ok(report)
    }

    // --- handler registration -------------------------------------------------

    /// Register the mail handler on the dispatcher and spawn its sequential
    /// consumer task: `MailSpool` reactions (and direct [`Self::send`]
    /// hints) route the *whole recipient spool* one file at a time
    /// (hints + sorted scans — per-recipient FIFO, bounded concurrency =
    /// 1). Other reaction variants are ignored here — later sprints
    /// register their own handlers.
    pub fn attach(self: &Arc<Self>, dispatcher: &Dispatcher) -> tokio::task::JoinHandle<()> {
        let mut rx = dispatcher.register("mailroute");
        let (hint_tx, mut hint_rx) = mpsc::unbounded_channel::<SafeName>();
        *self.hints.lock().unwrap_or_else(|e| e.into_inner()) = Some(hint_tx);
        let router = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                // Single sequential consumer: reaction hints and send
                // hints funnel into the same sorted spool scan.
                let recipient = tokio::select! {
                    item = rx.recv() => match item {
                        Some(item) => match item.reaction {
                            Reaction::MailSpool { recipient, .. } => recipient,
                            _ => continue,
                        },
                        None => break, // dispatcher gone — plane shutting down
                    },
                    hint = hint_rx.recv() => match hint {
                        // The router holds the sender for the task's whole
                        // life (it is Arc'd into this task), so the channel
                        // never closes while we loop.
                        Some(recipient) => recipient,
                        None => break,
                    },
                };
                if let Err(e) = router.route_recipient(&recipient) {
                    // Route errors leave files pending: the next hint or
                    // restart recovery retries (never dropped).
                    eprintln!(
                        "hamlet-daemon: mail spool scan {} failed: {e}",
                        recipient.as_str()
                    );
                }
            }
        })
    }

    /// Best-effort sled trace (telemetry loss degrades, never corrupts —
    /// ADR-0005 cl. 6). Detail carries names/ids only, never mail content
    /// beyond header fields (CONVENTIONS §6).
    fn trace(&self, agent: AgentKey, event: &str, detail: impl Into<String>) {
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: event.to_owned(),
            detail: detail.into(),
        };
        if let Err(e) = self.supervisor.telemetry().trace_event(agent, record) {
            eprintln!("hamlet-daemon: mail trace {event} failed: {e}");
        }
    }
}

/// Bring up the whole mail plane: router + sequential handler task +
/// tree watcher whose startup hook is the spool recovery scan (recovery
/// strictly before the first dispatched event — no route races).
///
/// # Errors
///
/// Watcher startup failures.
pub fn spawn_mail_plane(
    supervisor: Arc<Supervisor>,
    dispatcher: Arc<Dispatcher>,
) -> Result<(Arc<MailRouter>, WatcherHandle), MailRouteError> {
    let router = Arc::new(MailRouter::new(supervisor));
    router.attach(&dispatcher);
    let startup_router = Arc::clone(&router);
    let handle = spawn_tree_watcher(router.tree().clone(), Arc::clone(&dispatcher), move || {
        if let Err(e) = startup_router.ensure_spool_dirs() {
            eprintln!("hamlet-daemon: spool dir ensure failed: {e}");
        }
        match startup_router.recover_spools() {
            Ok(report) => {
                if report.scanned > 0 {
                    eprintln!(
                        "hamlet-daemon: mail recovery scanned={} delivered={} bounced={}",
                        report.scanned, report.delivered, report.bounced
                    );
                }
            }
            Err(e) => eprintln!("hamlet-daemon: mail recovery failed: {e}"),
        }
    })?;
    Ok((router, handle))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The routable slot uid behind a parsed mail's `From:` (`None` for named
/// senders — their bounces die with a trace, which bounds bounce chains).
fn mail_sender_uid(mail: &Mail) -> Option<u32> {
    match mail.header.from {
        MailSender::Uid(uid) => Some(uid),
        MailSender::Name(_) => None,
    }
}

/// Bounce body: reason + original headers only — never the original body
/// (ADR-0019 cl. 4; mail content may be sensitive, CONVENTIONS §6).
fn bounce_body(reason: &BounceReason, original: Option<&MailHeader>) -> String {
    let mut body = format!("Your message could not be delivered.\n\nReason: {reason}\n");
    if let Some(header) = original {
        body.push_str("\nOriginal headers:\n");
        body.push_str(&format!("From: {}\n", header.from));
        body.push_str(&format!("To: {}\n", header.to));
        body.push_str(&format!("Subject: {}\n", header.subject));
        body.push_str(&format!("Date: {}\n", header.date));
    }
    body
}

/// Ack target name: attempt 0 keeps the original; collisions get `.r<n>`
/// before the `.mail` extension (ADR-0019 cl. 3).
fn ack_file_name(file: &str, attempt: u32) -> String {
    if attempt == 0 {
        return file.to_owned();
    }
    match file.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.r{attempt}.{ext}"),
        None => format!("{file}.r{attempt}"),
    }
}

/// Sorted visible names of one directory's entries (`BTreeMap`-grade
/// determinism, CONVENTIONS §3.1); a missing directory scans empty.
fn sorted_dir_names(dir: &Path) -> Result<Vec<String>, MailRouteError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(MailRouteError::Io {
                context: format!("scan {}", dir.display()),
                source,
            })
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| MailRouteError::Io {
            context: format!("scan {}", dir.display()),
            source,
        })?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// The `system` sender identity (a valid §4 safe name — constant-checked
/// by the unit tests below).
fn system_name() -> SafeName {
    hamlet_core::sanitize::sanitize_name(SYSTEM_SENDER).expect("'system' is a valid safe name")
}

/// Opaque default `Date:` lineage token (unix seconds; never parsed into
/// state — CONVENTIONS §3.4).
fn default_date_token() -> String {
    format!("unix-{}", unix_now_ms() / 1000)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize(s: &str) -> SafeName {
        hamlet_core::sanitize::sanitize_name(s).unwrap()
    }

    #[test]
    fn ack_collision_suffixes_golden() {
        assert_eq!(
            ack_file_name("000000000007-1042-ab.mail", 0),
            "000000000007-1042-ab.mail"
        );
        assert_eq!(
            ack_file_name("000000000007-1042-ab.mail", 1),
            "000000000007-1042-ab.r1.mail"
        );
        assert_eq!(
            ack_file_name("000000000007-1042-ab.mail", 12),
            "000000000007-1042-ab.r12.mail"
        );
        assert_eq!(ack_file_name("foreign", 3), "foreign.r3");
    }

    #[test]
    fn actor_identities_are_daemon_asserted() {
        assert_eq!(MailActor::Mayor.sender(), MailSender::Uid(MAYOR_UID));
        assert_eq!(MailActor::Agent(1042).sender(), MailSender::Uid(1042));
        assert_eq!(MailActor::System.sender(), MailSender::Name(system_name()));
        let patron = MailActor::Patron(sanitize("anna"));
        assert_eq!(patron.sender(), MailSender::Name(sanitize("anna")));
        assert_eq!(patron.label(), "anna");
        assert_eq!(MailActor::System.writer_uid(), None);
        assert_eq!(MailActor::Agent(1042).writer_uid(), Some(1042));
    }

    #[test]
    fn bounce_body_never_carries_original_body() {
        let header = MailHeader {
            from: MailSender::Uid(1042),
            to: 1001,
            subject: "Secret plan".to_owned(),
            date: "unix-1".to_owned(),
        };
        let reason = BounceReason::NoSlot { uid: 1001 };
        let body = bounce_body(&reason, Some(&header));
        assert!(body.contains("no slot allocated for UID 1001"));
        assert!(body.contains("Subject: Secret plan"));
        assert!(!body.contains("Body"));
        let bare = bounce_body(&reason, None);
        assert!(bare.contains("Reason:"));
    }
}
