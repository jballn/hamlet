//! Context packet schema — the Mayor's whole prompt (SPEC §7; ADR-0027).
//!
//! The Mayor is a **stateless event processor**: it keeps no chat thread, so
//! the daemon reconstitutes everything it may know on every wake. A
//! [`ContextPacket`] is that reconstitution, and it is deliberately the *only*
//! prompt input:
//!
//! 1. `MAYOR.md` verbatim (the trunk behavior prompt, SPEC §2),
//! 2. the charter **state grid** — a typed, sorted, human-readable
//!    projection ([`StateGrid`]), never the raw `charter.json` text,
//! 3. the one targeted **event artifact** that caused this wake
//!    ([`EventArtifact`]) with its bounded body, and
//! 4. the **wake reason** ([`WakeReason`]), derived from the artifact so the
//!    two can never disagree.
//!
//! **Single-turn by construction:** there is no history field, no "previous
//! messages", and no accumulation buffer — a packet is immutable, rendered
//! once, and dropped when the turn ends. Rendering is a pure function of the
//! typed fields, so the same state always produces the same bytes
//! (CONVENTIONS §3 determinism spirit).
//!
//! The daemon owns wake *scheduling*, file reads, and session I/O; this module
//! owns only the schema and its rendering (purity seam, CONVENTIONS §2).

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::charter::{Charter, CommissionStatus, DecreeStatus};
use crate::decree::{DecreeId, TreeRelPath};
use crate::freight::ViolationKind;
use crate::mail::MailSender;
use crate::sanitize::SafeName;
use crate::session::{CorrelationId, SessionStatus};
use crate::slots::SlotStatus;

/// Packet schema version rendered in the header (ADR-0027).
pub const PACKET_VERSION: u32 = 1;
/// Maximum accepted `MAYOR.md` size, bytes.
pub const MAX_MAYOR_MD_BYTES: usize = 256 * 1024;
/// Maximum accepted event-artifact body, bytes (the same cap as a mail
/// body, ADR-0019 cl. 1).
pub const MAX_ARTIFACT_BYTES: usize = 64 * 1024;
/// Maximum rendered packet size, bytes — a hard refusal rather than a
/// truncated prompt (a truncated Mayor prompt is a corrupted one).
pub const MAX_PACKET_BYTES: usize = 512 * 1024;

/// Why the Mayor is awake. Derived from the event artifact by
/// [`ContextPacket::new`], never supplied independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeReason {
    /// Inbound mail addressed to the Mayor's slot (SPEC §8.1).
    MailToMayor,
    /// An accepted inbound webhook matched a Mayor-relevant topic
    /// (SPEC §8.5).
    WebhookEvent,
    /// The freight audit escalated a sandbox violation (SPEC §4, §7).
    FreightViolation,
    /// A decree passed `config.json`→`mayor.ack_timeout_ms` without an
    /// acknowledgment file (ADR-0026 cl. 5).
    DecreeAckTimeout,
    /// A new `commission<n>/` appeared (SPEC §9.1).
    CommissionCreated,
    /// A commission work branch advanced and a diff is ready to evaluate
    /// (SPEC §7 validation loop).
    GitDiffReady,
    /// An agent session crashed in a way the restart policy cannot absorb
    /// (or every crash, under `mayor.crash_wake: every` — ADR-0029 cl. 2).
    SessionCrashed,
}

impl WakeReason {
    /// The stable wire token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::MailToMayor => "mail_to_mayor",
            Self::WebhookEvent => "webhook_event",
            Self::FreightViolation => "freight_violation",
            Self::DecreeAckTimeout => "decree_ack_timeout",
            Self::CommissionCreated => "commission_created",
            Self::GitDiffReady => "git_diff_ready",
            Self::SessionCrashed => "session_crashed",
        }
    }
}

impl fmt::Display for WakeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// The typed event that woke the Mayor (SPEC §7). Exactly one per packet —
/// a wake is always *caused* by something observable in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// A mail spool file arrived for the Mayor's slot.
    MailReceived {
        /// Claimed sender identity (ADR-0019 cl. 2 trust model).
        from: MailSender,
        /// Single-line subject.
        subject: String,
        /// Village-relative spool file the body came from.
        spool: TreeRelPath,
    },
    /// An inbound webhook event was accepted for a Mayor-relevant topic.
    WebhookEvent {
        /// Sanitized topic name.
        topic: SafeName,
        /// Dispatch correlation id of the inbox envelope (ADR-0022 cl. 4).
        correlation: CorrelationId,
        /// Upstream HTTP status, or 202 for daemon-accepted envelopes.
        status: u16,
    },
    /// The freight audit escalated violations (ADR-0021 cl. 5).
    FreightViolation {
        /// Offending sandbox name.
        sandbox: SafeName,
        /// Charter slot that owns the sandbox.
        owner: u32,
        /// Violation kinds, in audit order.
        kinds: Vec<ViolationKind>,
    },
    /// A decree went unacknowledged past the configured timeout.
    DecreeAckTimeout {
        /// The decree awaiting an ack file.
        decree_id: DecreeId,
        /// How long the daemon waited before waking (telemetry-shaped, never
        /// a state input — CONVENTIONS §3 r4).
        waited_ms: u64,
    },
    /// A new commission appeared with its `COMMISSION.md`.
    CommissionCreated {
        /// Ledger index (`commission<n>`, n ≥ 1).
        commission: u64,
    },
    /// A work branch advanced; the body carries the unified diff.
    GitDiffReady {
        /// Ledger index.
        commission: u64,
        /// Base ref name.
        base: SafeName,
        /// Head ref name.
        head: SafeName,
    },
    /// A supervised agent session ended unexpectedly (SPEC §6). Which
    /// crashes reach the Mayor is `config.json` → `mayor.crash_wake`
    /// (ADR-0029 cl. 2); the default reports only the ones the daemon
    /// cannot restart on its own.
    SessionCrashed {
        /// Charter slot whose session died.
        uid: u32,
        /// The dead life (u16 uid + incarnation, no clock — SPEC §6).
        session: crate::session::SessionId,
        /// Consecutive auto-restarts already spent before this crash.
        restarts: u32,
        /// True when the restart budget is exhausted and the daemon will
        /// not revive the agent (the `TITLE.md` lineage stamp of a dead
        /// parcel).
        permanent: bool,
    },
    /// Several events that share one wake reason, merged into the single
    /// evaluation they deserve (ADR-0028 cl. 2). The packet still carries
    /// **one** artifact — [`ContextPacket`] is single-turn by construction —
    /// so merging moves several artifacts *into* it: deterministically, in
    /// queue order, and only while the merged body fits the artifact cap.
    MergedBatch {
        /// The reason every member shares — why this wake happens at all.
        reason: WakeReason,
        /// Members in queue (FIFO) order; never empty, never mixed-reason.
        members: Vec<BatchMember>,
    },
}

/// One event inside an [`EventKind::MergedBatch`]: its typed kind (rendered
/// per member, so no member is anonymous) plus its verbatim bounded body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMember {
    /// The member event's typed kind.
    pub kind: EventKind,
    /// The member's verbatim body (already capped on the way in).
    pub body: String,
}

impl EventKind {
    /// The reason a packet carrying this event is being built.
    #[must_use]
    pub fn wake_reason(&self) -> WakeReason {
        match self {
            Self::MailReceived { .. } => WakeReason::MailToMayor,
            Self::WebhookEvent { .. } => WakeReason::WebhookEvent,
            Self::FreightViolation { .. } => WakeReason::FreightViolation,
            Self::DecreeAckTimeout { .. } => WakeReason::DecreeAckTimeout,
            Self::CommissionCreated { .. } => WakeReason::CommissionCreated,
            Self::GitDiffReady { .. } => WakeReason::GitDiffReady,
            Self::SessionCrashed { .. } => WakeReason::SessionCrashed,
            Self::MergedBatch { reason, .. } => *reason,
        }
    }

    /// The typed `key: value` header block rendered under `--- EVENT ---`.
    /// Deterministic, one line per field, no free text.
    #[must_use]
    pub fn header_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("kind: {}", self.wake_reason())];
        match self {
            Self::MailReceived {
                from,
                subject,
                spool,
            } => {
                lines.push(format!("from: {from}"));
                lines.push(format!("subject: {subject}"));
                lines.push(format!("spool: {spool}"));
            }
            Self::WebhookEvent {
                topic,
                correlation,
                status,
            } => {
                lines.push(format!("topic: {topic}"));
                lines.push(format!("correlation: {correlation}"));
                lines.push(format!("status: {status}"));
            }
            Self::FreightViolation {
                sandbox,
                owner,
                kinds,
            } => {
                lines.push(format!("sandbox: {sandbox}"));
                lines.push(format!("owner: {owner}"));
                lines.push(format!(
                    "violations: {}",
                    kinds
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            Self::DecreeAckTimeout {
                decree_id,
                waited_ms,
            } => {
                lines.push(format!("decree_id: {decree_id}"));
                lines.push(format!("waited_ms: {waited_ms}"));
            }
            Self::CommissionCreated { commission } => {
                lines.push(format!("commission: {commission}"));
            }
            Self::GitDiffReady {
                commission,
                base,
                head,
            } => {
                lines.push(format!("commission: {commission}"));
                lines.push(format!("base: {base}"));
                lines.push(format!("head: {head}"));
            }
            Self::SessionCrashed {
                uid,
                session,
                restarts,
                permanent,
            } => {
                lines.push(format!("uid: {uid}"));
                lines.push(format!("session: {session}"));
                lines.push(format!("restarts: {restarts}"));
                lines.push(format!(
                    "permanent: {permanent}",
                    permanent = if *permanent { "yes" } else { "no" }
                ));
            }
            Self::MergedBatch { reason, members } => {
                lines.push(format!("merged: {}", members.len()));
                for (index, member) in members.iter().enumerate() {
                    lines.push(format!("member {}:", index + 1));
                    for line in member.kind.header_lines() {
                        lines.push(format!("  {line}"));
                    }
                }
                debug_assert!(
                    members.iter().all(|m| m.kind.wake_reason() == *reason) && !members.is_empty(),
                    "a merged batch is homogeneous and non-empty by construction"
                );
            }
        }
        lines
    }
}

/// One event artifact: its typed [`EventKind`] plus the bounded verbatim body
/// the Mayor must read (mail text, webhook envelope, quarantine flag,
/// `COMMISSION.md`, unified diff). The body is data, never re-parsed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventArtifact {
    /// What happened.
    pub kind: EventKind,
    /// Verbatim artifact text, 1–[`MAX_ARTIFACT_BYTES`] bytes.
    pub body: String,
}

impl EventArtifact {
    /// Build an artifact, enforcing the body cap.
    ///
    /// # Errors
    ///
    /// [`PacketError::EmptyArtifact`] or [`PacketError::ArtifactTooLarge`].
    pub fn new(kind: EventKind, body: impl Into<String>) -> Result<Self, PacketError> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(PacketError::EmptyArtifact);
        }
        if body.len() > MAX_ARTIFACT_BYTES {
            return Err(PacketError::ArtifactTooLarge { bytes: body.len() });
        }
        Ok(Self { kind, body })
    }

    /// Merge same-reason events into **one** artifact (ADR-0028 cl. 2):
    /// five mails waiting in the queue become a single evaluation carrying
    /// all five, not five evaluations.
    ///
    /// The merged body is the members' bodies verbatim, each behind a
    /// `--- artifact <n> ---` fence; the typed headers stay per member in
    /// [`EventKind::header_lines`], so nothing is merged into anonymity.
    /// Refusals, never truncation: a batch that would exceed
    /// [`MAX_ARTIFACT_BYTES`] is rejected here and the queue splits it
    /// across further wakes instead of shortening a prompt.
    ///
    /// # Errors
    ///
    /// - [`PacketError::EmptyArtifact`] — no members, or all bodies blank;
    /// - [`PacketError::MixedWakeReasons`] — members disagree on the reason
    ///   (a merged wake must have a single cause);
    /// - [`PacketError::ArtifactTooLarge`] — the combined body is too big.
    pub fn merged(reason: WakeReason, members: Vec<BatchMember>) -> Result<Self, PacketError> {
        if members.is_empty() {
            return Err(PacketError::EmptyArtifact);
        }
        if let Some(found) = members
            .iter()
            .map(|m| m.kind.wake_reason())
            .find(|r| *r != reason)
        {
            return Err(PacketError::MixedWakeReasons {
                expected: reason,
                found,
            });
        }
        let mut body = String::new();
        for (index, member) in members.iter().enumerate() {
            if index > 0 {
                body.push('\n');
            }
            body.push_str(&format!("--- artifact {} ---\n", index + 1));
            body.push_str(member.body.trim_end_matches('\n'));
            body.push('\n');
        }
        if body.trim().is_empty() {
            return Err(PacketError::EmptyArtifact);
        }
        if body.len() > MAX_ARTIFACT_BYTES {
            return Err(PacketError::ArtifactTooLarge { bytes: body.len() });
        }
        Ok(Self {
            kind: EventKind::MergedBatch { reason, members },
            body,
        })
    }
}

/// One charter slot row in the state grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotRow {
    /// Permanent identity holding the slot.
    pub identity: SafeName,
    /// Slot status token.
    pub status: &'static str,
    /// Commission the slot is bound to, if any.
    pub commission: Option<u64>,
}

/// One supervised session row in the state grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    /// Restart counter (incarnation − 1) of the current life.
    pub restart_count: u32,
    /// Session status token.
    pub status: &'static str,
}

/// One parcel lease row in the state grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParcelRow {
    /// Identity holding the lease.
    pub identity: SafeName,
    /// True while the home is parked in stasis (SPEC §10).
    pub stasis: bool,
}

/// The human-readable charter projection handed to the Mayor (ADR-0027
/// cl. 2): a **subset**, not the charter dump — the Mayor needs slots,
/// commissions, open parcels, live sessions, and the decree cursor.
///
/// Rendering is canonical pretty JSON with sorted keys at every level
/// (`BTreeMap` groups), which makes the goldens byte-stable and keeps the
/// packet readable ("human-readable projection", SPEC §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateGrid {
    /// Charter schema version the projection was taken from.
    pub charter_version: u32,
    /// The sequence the *next* emitted decree must use (ADR-0026 cl. 3).
    pub next_decree_seq: u64,
    /// Decrees still queued (`Queued`); terminal ones are history.
    pub open_decrees: BTreeMap<u64, DecreeStatus>,
    /// Commission index → status.
    pub commissions: BTreeMap<u64, CommissionStatus>,
    /// UID → parcel lease row.
    pub parcels: BTreeMap<u32, ParcelRow>,
    /// UID → supervised session row.
    pub sessions: BTreeMap<u32, SessionRow>,
    /// UID → slot row.
    pub slots: BTreeMap<u32, SlotRow>,
}

impl StateGrid {
    /// Project a charter into the Mayor's state grid. Pure; sorted by
    /// construction (`BTreeMap`).
    #[must_use]
    pub fn from_charter(charter: &Charter) -> Self {
        Self {
            charter_version: charter.charter_version,
            next_decree_seq: charter.next_decree_seq(),
            open_decrees: charter
                .decrees
                .iter()
                .filter(|(_, status)| **status == DecreeStatus::Queued)
                .map(|(seq, status)| (*seq, *status))
                .collect(),
            commissions: charter.commissions.clone(),
            parcels: charter
                .parcels
                .iter()
                .map(|(uid, lease)| {
                    (
                        *uid,
                        ParcelRow {
                            identity: lease.identity.clone(),
                            stasis: lease.stasis,
                        },
                    )
                })
                .collect(),
            sessions: charter
                .sessions
                .iter()
                .map(|(uid, session)| {
                    (
                        *uid,
                        SessionRow {
                            restart_count: session.restart_count,
                            status: session_status_token(session.status),
                        },
                    )
                })
                .collect(),
            slots: charter
                .slots
                .iter()
                .map(|(uid, record)| {
                    (
                        *uid,
                        SlotRow {
                            identity: record.identity.clone(),
                            status: slot_status_token(record.status),
                            commission: record.commission,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Canonical pretty-JSON rendering (sorted keys, 2-space, trailing
    /// newline) — the bytes embedded in the packet's grid section.
    #[must_use]
    pub fn render(&self) -> String {
        let value = serde_json::to_value(self).expect("BTreeMap-based StateGrid is infallible");
        let mut out =
            serde_json::to_string_pretty(&value).expect("Value pretty-printing is infallible");
        out.push('\n');
        out
    }
}

fn slot_status_token(status: SlotStatus) -> &'static str {
    match status {
        SlotStatus::Active => "active",
        SlotStatus::Hibernated => "hibernated",
        SlotStatus::Crashed => "crashed",
    }
}

fn session_status_token(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Busy => "busy",
        SessionStatus::Idle => "idle",
        SessionStatus::Stopped => "stopped",
        SessionStatus::Crashed => "crashed",
        SessionStatus::Hibernating => "hibernating",
    }
}

/// Why a packet could not be built or rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketError {
    /// `MAYOR.md` was empty or blank.
    EmptyMayorMd,
    /// `MAYOR.md` exceeds [`MAX_MAYOR_MD_BYTES`].
    MayorMdTooLarge {
        /// Observed size, bytes.
        bytes: usize,
    },
    /// The event body was empty/blank.
    EmptyArtifact,
    /// The event body exceeds [`MAX_ARTIFACT_BYTES`].
    ArtifactTooLarge {
        /// Observed size, bytes.
        bytes: usize,
    },
    /// The rendered packet exceeds [`MAX_PACKET_BYTES`] — refused, never
    /// truncated.
    PacketTooLarge {
        /// Observed size, bytes.
        bytes: usize,
    },
    /// A merged batch carried members with different wake reasons — a wake
    /// has exactly one cause, so the batch is refused rather than relabeled.
    MixedWakeReasons {
        /// The reason the batch declared.
        expected: WakeReason,
        /// The first reason that disagreed with it.
        found: WakeReason,
    },
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMayorMd => write!(f, "MAYOR.md must not be empty"),
            Self::MayorMdTooLarge { bytes } => write!(
                f,
                "MAYOR.md exceeds {MAX_MAYOR_MD_BYTES} bytes (got {bytes})"
            ),
            Self::EmptyArtifact => write!(f, "event artifact body must not be empty"),
            Self::ArtifactTooLarge { bytes } => write!(
                f,
                "event artifact exceeds {MAX_ARTIFACT_BYTES} bytes (got {bytes})"
            ),
            Self::PacketTooLarge { bytes } => write!(
                f,
                "rendered context packet exceeds {MAX_PACKET_BYTES} bytes (got {bytes})"
            ),
            Self::MixedWakeReasons { expected, found } => write!(
                f,
                "merged batch declared wake reason {expected} but carries a member with {found}"
            ),
        }
    }
}

impl std::error::Error for PacketError {}

/// The complete, single-turn Mayor prompt (SPEC §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPacket {
    /// `MAYOR.md` verbatim.
    pub mayor_md: String,
    /// Charter projection.
    pub state_grid: StateGrid,
    /// The one event that caused this wake.
    pub event: EventArtifact,
    /// Derived from `event.kind` by [`ContextPacket::new`].
    pub wake_reason: WakeReason,
}

impl ContextPacket {
    /// Assemble a packet, deriving [`ContextPacket::wake_reason`] from the
    /// event and enforcing the input caps.
    ///
    /// # Errors
    ///
    /// [`PacketError::EmptyMayorMd`], [`PacketError::MayorMdTooLarge`].
    pub fn new(
        mayor_md: impl Into<String>,
        state_grid: StateGrid,
        event: EventArtifact,
    ) -> Result<Self, PacketError> {
        let mayor_md = mayor_md.into();
        if mayor_md.trim().is_empty() {
            return Err(PacketError::EmptyMayorMd);
        }
        if mayor_md.len() > MAX_MAYOR_MD_BYTES {
            return Err(PacketError::MayorMdTooLarge {
                bytes: mayor_md.len(),
            });
        }
        let wake_reason = event.kind.wake_reason();
        Ok(Self {
            mayor_md,
            state_grid,
            event,
            wake_reason,
        })
    }

    /// Render the packet — the entire prompt for exactly one Mayor turn.
    ///
    /// Section order is fixed: header/wake reason, `MAYOR.md`, state grid,
    /// event header, artifact body, decree protocol, end marker. There is no
    /// history section, so nothing can accumulate between turns (SPEC §7).
    ///
    /// # Errors
    ///
    /// [`PacketError::PacketTooLarge`] — refused, never truncated.
    pub fn render(&self) -> Result<String, PacketError> {
        let mut out = String::new();
        out.push_str(&format!(
            "=== HAMLET EXECUTIVE PACKET v{PACKET_VERSION} ===\n"
        ));
        out.push_str(&format!("wake_reason: {}\n", self.wake_reason));
        out.push_str("\n--- MAYOR.md ---\n");
        out.push_str(self.mayor_md.trim_end_matches('\n'));
        out.push('\n');
        out.push_str("\n--- CHARTER STATE GRID ---\n");
        out.push_str(&self.state_grid.render());
        out.push_str("\n--- EVENT ---\n");
        for line in self.event.kind.header_lines() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("\n--- ARTIFACT BODY ---\n");
        out.push_str(self.event.body.trim_end_matches('\n'));
        out.push('\n');
        out.push_str("\n--- DECREE PROTOCOL ---\n");
        out.push_str(&format!(
            "decree_dir: {}\n\
             file_name: <decree_id>.json\n\
             decree_id: d-<seq:012>-<digest16>\n\
             next_decree_seq: {}\n\
             digest: fnv1a64 hex of the canonical body with decree_id removed\n\
             seq_rule: must equal next_decree_seq; the charter counter orders decrees, never a clock\n\
             ack: the daemon writes <decree_id>.ack next to the decree (done | rejected+reason)\n\
             emission: one turn, one decision — write decree files only, never podman or host commands\n",
            DECREE_DIR,
            self.state_grid.next_decree_seq
        ));
        out.push_str("\n=== END OF PACKET ===\n");
        if out.len() > MAX_PACKET_BYTES {
            return Err(PacketError::PacketTooLarge { bytes: out.len() });
        }
        Ok(out)
    }
}

/// The decree directory as the Mayor sees it inside its container
/// (`/home/1000/decrees/`); the host path is
/// `keep/census/manor/home/decrees/` (ADR-0026 cl. 2).
pub const DECREE_DIR: &str = "/home/1000/decrees";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charter::{AgentRef, ParcelLease};
    use crate::sanitize::sanitize_name;
    use crate::session::{SessionId, SessionRecord};
    use crate::slots::SlotRecord;

    fn name(s: &str) -> SafeName {
        sanitize_name(s).unwrap()
    }

    const MAYOR_MD: &str = "# Mayor\n\nYou are the executive. Emit decrees as files.\n";

    /// A small but complete charter for the grid projection.
    fn sample_charter() -> Charter {
        let mut charter = Charter::empty();
        charter.reserve_mayor(name("manor")).unwrap();
        let ada = charter.allocate_slot(name("ada"), Some(1)).unwrap();
        let grace = charter.allocate_slot(name("grace"), None).unwrap();
        charter.commissions.insert(1, CommissionStatus::Open);
        charter.commissions.insert(2, CommissionStatus::Closed);
        charter.agents.insert(name("ada"), AgentRef { slot: ada });
        charter.parcels.insert(
            ada,
            ParcelLease {
                identity: name("ada"),
                stasis: false,
            },
        );
        charter.sessions.insert(
            ada,
            SessionRecord {
                id: SessionId::new(ada, 0).unwrap(),
                status: SessionStatus::Busy,
                restart_count: 1,
            },
        );
        charter
            .slots
            .insert(grace, SlotRecord::new(name("grace"), None));
        crate::slots::hibernate(&mut charter.slots, grace).unwrap();
        charter
            .decrees
            .insert(1, crate::charter::DecreeStatus::Delivered);
        charter
            .decree_receipts
            .insert(1, "c0225c3b8fabf543".to_string());
        charter
            .decrees
            .insert(2, crate::charter::DecreeStatus::Queued);
        charter
            .decree_receipts
            .insert(2, "0000000000000002".to_string());
        charter
    }

    fn mail_event() -> EventArtifact {
        EventArtifact::new(
            EventKind::MailReceived {
                from: MailSender::Uid(1001),
                subject: "Freight report ready".to_string(),
                spool: TreeRelPath::parse("courier/mail/1000/000000000007-1001-ab12.mail").unwrap(),
            },
            "Body of the mail.\nSecond line.\n",
        )
        .unwrap()
    }

    /// Committed golden vector: the exact charter projection the Mayor sees.
    #[test]
    fn state_grid_render_golden() {
        let grid = StateGrid::from_charter(&sample_charter());
        let expected = "\
{
  \"charter_version\": 3,
  \"commissions\": {
    \"1\": \"open\",
    \"2\": \"closed\"
  },
  \"next_decree_seq\": 3,
  \"open_decrees\": {
    \"2\": \"queued\"
  },
  \"parcels\": {
    \"1001\": {
      \"identity\": \"ada\",
      \"stasis\": false
    }
  },
  \"sessions\": {
    \"1001\": {
      \"restart_count\": 1,
      \"status\": \"busy\"
    }
  },
  \"slots\": {
    \"1000\": {
      \"commission\": null,
      \"identity\": \"manor\",
      \"status\": \"active\"
    },
    \"1001\": {
      \"commission\": 1,
      \"identity\": \"ada\",
      \"status\": \"active\"
    },
    \"1002\": {
      \"commission\": null,
      \"identity\": \"grace\",
      \"status\": \"hibernated\"
    }
  }
}
";
        assert_eq!(grid.render(), expected);
        // only queued decrees are projected as open (delivered is history)
        assert_eq!(grid.open_decrees.len(), 1);
        assert_eq!(grid.next_decree_seq, 3);
    }

    /// An empty village renders a minimal but complete grid.
    #[test]
    fn empty_charter_grid_is_minimal() {
        assert_eq!(
            StateGrid::from_charter(&Charter::empty()).render(),
            "\
{
  \"charter_version\": 3,
  \"commissions\": {},
  \"next_decree_seq\": 1,
  \"open_decrees\": {},
  \"parcels\": {},
  \"sessions\": {},
  \"slots\": {}
}
"
        );
    }

    /// Committed golden vector: a full packet render (mail wake).
    #[test]
    fn context_packet_render_golden() {
        let packet = ContextPacket::new(
            MAYOR_MD,
            StateGrid::from_charter(&sample_charter()),
            mail_event(),
        )
        .unwrap();
        assert_eq!(packet.wake_reason, WakeReason::MailToMayor);
        let expected = "\
=== HAMLET EXECUTIVE PACKET v1 ===
wake_reason: mail_to_mayor

--- MAYOR.md ---
# Mayor

You are the executive. Emit decrees as files.

--- CHARTER STATE GRID ---
{
  \"charter_version\": 3,
  \"commissions\": {
    \"1\": \"open\",
    \"2\": \"closed\"
  },
  \"next_decree_seq\": 3,
  \"open_decrees\": {
    \"2\": \"queued\"
  },
  \"parcels\": {
    \"1001\": {
      \"identity\": \"ada\",
      \"stasis\": false
    }
  },
  \"sessions\": {
    \"1001\": {
      \"restart_count\": 1,
      \"status\": \"busy\"
    }
  },
  \"slots\": {
    \"1000\": {
      \"commission\": null,
      \"identity\": \"manor\",
      \"status\": \"active\"
    },
    \"1001\": {
      \"commission\": 1,
      \"identity\": \"ada\",
      \"status\": \"active\"
    },
    \"1002\": {
      \"commission\": null,
      \"identity\": \"grace\",
      \"status\": \"hibernated\"
    }
  }
}

--- EVENT ---
kind: mail_to_mayor
from: 1001
subject: Freight report ready
spool: courier/mail/1000/000000000007-1001-ab12.mail

--- ARTIFACT BODY ---
Body of the mail.
Second line.

--- DECREE PROTOCOL ---
decree_dir: /home/1000/decrees
file_name: <decree_id>.json
decree_id: d-<seq:012>-<digest16>
next_decree_seq: 3
digest: fnv1a64 hex of the canonical body with decree_id removed
seq_rule: must equal next_decree_seq; the charter counter orders decrees, never a clock
ack: the daemon writes <decree_id>.ack next to the decree (done | rejected+reason)
emission: one turn, one decision — write decree files only, never podman or host commands

=== END OF PACKET ===
";
        assert_eq!(packet.render().unwrap(), expected);
    }

    /// Every event kind renders its typed header block and its derived wake
    /// reason (golden table).
    #[test]
    fn event_header_table_golden() {
        let cases: Vec<(EventKind, WakeReason, Vec<&str>)> = vec![
            (
                EventKind::MailReceived {
                    from: MailSender::Name(name("system")),
                    subject: "Escalation".to_string(),
                    spool: TreeRelPath::parse("courier/mail/1000/000000000009-system-aa.mail")
                        .unwrap(),
                },
                WakeReason::MailToMayor,
                vec![
                    "kind: mail_to_mayor",
                    "from: system",
                    "subject: Escalation",
                    "spool: courier/mail/1000/000000000009-system-aa.mail",
                ],
            ),
            (
                EventKind::WebhookEvent {
                    topic: name("github"),
                    correlation: CorrelationId::parse("d-9-1000.0").unwrap(),
                    status: 202,
                },
                WakeReason::WebhookEvent,
                vec![
                    "kind: webhook_event",
                    "topic: github",
                    "correlation: d-9-1000.0",
                    "status: 202",
                ],
            ),
            (
                EventKind::FreightViolation {
                    sandbox: name("exchange"),
                    owner: 1042,
                    kinds: vec![ViolationKind::WorldWritable, ViolationKind::ForeignUid],
                },
                WakeReason::FreightViolation,
                vec![
                    "kind: freight_violation",
                    "sandbox: exchange",
                    "owner: 1042",
                    "violations: world_writable,foreign_uid",
                ],
            ),
            (
                EventKind::DecreeAckTimeout {
                    decree_id: DecreeId::new(3, "0000000000000003").unwrap(),
                    waited_ms: 30_000,
                },
                WakeReason::DecreeAckTimeout,
                vec![
                    "kind: decree_ack_timeout",
                    "decree_id: d-000000000003-0000000000000003",
                    "waited_ms: 30000",
                ],
            ),
            (
                EventKind::CommissionCreated { commission: 4 },
                WakeReason::CommissionCreated,
                vec!["kind: commission_created", "commission: 4"],
            ),
            (
                EventKind::GitDiffReady {
                    commission: 3,
                    base: name("main"),
                    head: name("agent-ada"),
                },
                WakeReason::GitDiffReady,
                vec![
                    "kind: git_diff_ready",
                    "commission: 3",
                    "base: main",
                    "head: agent-ada",
                ],
            ),
            (
                EventKind::SessionCrashed {
                    uid: 1042,
                    session: crate::session::SessionId::new(1042, 3).unwrap(),
                    restarts: 5,
                    permanent: true,
                },
                WakeReason::SessionCrashed,
                vec![
                    "kind: session_crashed",
                    "uid: 1042",
                    "session: 1042.3",
                    "restarts: 5",
                    "permanent: yes",
                ],
            ),
        ];
        for (kind, reason, expected_lines) in cases {
            assert_eq!(kind.wake_reason(), reason);
            assert_eq!(kind.header_lines(), expected_lines);
            let packet = ContextPacket::new(
                MAYOR_MD,
                StateGrid::from_charter(&sample_charter()),
                EventArtifact::new(kind, "artifact body\n").unwrap(),
            )
            .unwrap();
            let rendered = packet.render().unwrap();
            assert!(rendered.contains(&format!("wake_reason: {reason}\n")));
            assert!(rendered.contains(&expected_lines.join("\n")));
            assert!(rendered.ends_with("=== END OF PACKET ===\n"));
        }
    }

    /// Single-turn by construction: the packet is a pure function of its
    /// inputs, contains no history, and is unaffected by any previous turn.
    #[test]
    fn packet_is_deterministic_and_history_free() {
        let grid = StateGrid::from_charter(&sample_charter());
        let a = ContextPacket::new(MAYOR_MD, grid.clone(), mail_event()).unwrap();
        let b = ContextPacket::new(MAYOR_MD, grid, mail_event()).unwrap();
        assert_eq!(a.render().unwrap(), b.render().unwrap());
        let text = a.render().unwrap();
        for forbidden in ["history", "previous", "conversation", "thread"] {
            assert!(
                !text.to_lowercase().contains(forbidden),
                "packet must not carry a {forbidden} section"
            );
        }
        // exactly one turn marker / end marker
        assert_eq!(text.matches("=== HAMLET EXECUTIVE PACKET").count(), 1);
        assert_eq!(text.matches("=== END OF PACKET ===").count(), 1);
    }

    /// Merged wakes (ADR-0028 cl. 2): five mails become ONE artifact with
    /// five fenced bodies and per-member headers — golden, deterministic,
    /// and still a single-turn packet with no history.
    #[test]
    fn merged_batch_render_golden() {
        let member = |seq: u64, body: &str| BatchMember {
            kind: EventKind::MailReceived {
                from: MailSender::Uid(1001),
                subject: format!("Report {seq}"),
                spool: TreeRelPath::parse(&format!(
                    "courier/mail/1000/00000000000{seq}-1001-ab12.mail"
                ))
                .unwrap(),
            },
            body: body.to_owned(),
        };
        let packet = ContextPacket::new(
            MAYOR_MD,
            StateGrid::from_charter(&sample_charter()),
            EventArtifact::merged(
                WakeReason::MailToMayor,
                vec![
                    member(7, "first body\n"),
                    member(8, "second body\n"),
                    member(9, "third body"),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(packet.wake_reason, WakeReason::MailToMayor);
        let text = packet.render().unwrap();
        assert!(text.contains("kind: mail_to_mayor\nmerged: 3\nmember 1:\n  kind: mail_to_mayor\n  from: 1001\n  subject: Report 7\n"));
        assert!(text.contains("member 3:\n  kind: mail_to_mayor\n"));
        // every member body survives verbatim, fenced and in order
        let bodies: Vec<&str> = text
            .match_indices("--- artifact ")
            .map(|(at, _)| &text[at..])
            .take(3)
            .collect();
        assert_eq!(bodies.len(), 3);
        assert!(text.find("first body").unwrap() < text.find("second body").unwrap());
        assert!(text.find("second body").unwrap() < text.find("third body").unwrap());
        assert!(text.contains("--- artifact 1 ---\nfirst body\n\n--- artifact 2 ---\nsecond body\n\n--- artifact 3 ---\nthird body\n"));
    }

    /// Merging is a refusal boundary, never a truncation: mixed reasons and
    /// oversized combined bodies are typed errors so the queue splits the
    /// wake instead of shortening a prompt.
    #[test]
    fn merged_batch_refusals() {
        let mail = |subject: &str| BatchMember {
            kind: EventKind::MailReceived {
                from: MailSender::Uid(1001),
                subject: subject.to_owned(),
                spool: TreeRelPath::parse("courier/mail/1000/000000000007-1001-ab12.mail").unwrap(),
            },
            body: "body\n".to_owned(),
        };
        assert_eq!(
            EventArtifact::merged(WakeReason::MailToMayor, vec![]).unwrap_err(),
            PacketError::EmptyArtifact
        );
        let other = BatchMember {
            kind: EventKind::CommissionCreated { commission: 2 },
            body: "brief\n".to_owned(),
        };
        assert_eq!(
            EventArtifact::merged(
                WakeReason::MailToMayor,
                vec![mail("a"), mail("b"), other.clone()]
            )
            .unwrap_err(),
            PacketError::MixedWakeReasons {
                expected: WakeReason::MailToMayor,
                found: WakeReason::CommissionCreated,
            }
        );
        let big = EventArtifact::merged(
            WakeReason::MailToMayor,
            vec![
                BatchMember {
                    kind: mail("a").kind,
                    body: "x".repeat(MAX_ARTIFACT_BYTES / 2),
                },
                BatchMember {
                    kind: mail("b").kind,
                    body: "y".repeat(MAX_ARTIFACT_BYTES / 2),
                },
            ],
        )
        .unwrap_err();
        assert!(
            matches!(big, PacketError::ArtifactTooLarge { .. }),
            "{big:?}"
        );
    }

    /// Input caps are refusals, never truncations (CONVENTIONS §3 spirit:
    /// a truncated prompt is a corrupted one).
    #[test]
    fn caps_are_refused_not_truncated() {
        let grid = StateGrid::from_charter(&sample_charter());
        assert_eq!(
            ContextPacket::new("  \n", grid.clone(), mail_event()).unwrap_err(),
            PacketError::EmptyMayorMd
        );
        assert_eq!(
            ContextPacket::new(
                "m".repeat(MAX_MAYOR_MD_BYTES + 1),
                grid.clone(),
                mail_event()
            )
            .unwrap_err(),
            PacketError::MayorMdTooLarge {
                bytes: MAX_MAYOR_MD_BYTES + 1
            }
        );
        assert_eq!(
            EventArtifact::new(EventKind::CommissionCreated { commission: 1 }, "  \n").unwrap_err(),
            PacketError::EmptyArtifact
        );
        assert_eq!(
            EventArtifact::new(
                EventKind::CommissionCreated { commission: 1 },
                "x".repeat(MAX_ARTIFACT_BYTES + 1)
            )
            .unwrap_err(),
            PacketError::ArtifactTooLarge {
                bytes: MAX_ARTIFACT_BYTES + 1
            }
        );
        // At the exact bounds a packet still renders; MAX_PACKET_BYTES is a
        // backstop for future sections (the input caps sum below it today),
        // and it refuses rather than truncates when it ever trips.
        const { assert!(MAX_MAYOR_MD_BYTES + MAX_ARTIFACT_BYTES < MAX_PACKET_BYTES) };
        let at_bound = EventArtifact::new(
            EventKind::CommissionCreated { commission: 1 },
            "x".repeat(MAX_ARTIFACT_BYTES),
        )
        .unwrap();
        let rendered = ContextPacket::new("m".repeat(MAX_MAYOR_MD_BYTES), grid, at_bound)
            .unwrap()
            .render()
            .unwrap();
        assert!(rendered.len() < MAX_PACKET_BYTES);
        assert!(rendered.starts_with("=== HAMLET EXECUTIVE PACKET v1 ==="));
    }
}
