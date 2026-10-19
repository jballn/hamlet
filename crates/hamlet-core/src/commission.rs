//! Commission lifecycle (SPEC §9): the typed state machine, the per-commission
//! charter record, and the INDENTURE.md grammar (ADR-0036).
//!
//! A commission is the unit of delegated work: an operator creates it, the
//! Mayor decomposes it into per-agent indenture entries, the roster executes,
//! and the commission converges on one deliverable plus an optional promotion.
//!
//! # States
//!
//! ```text
//! Draft ──IndenturePosted──▶ Active ──AllEntriesDone──▶ WindingDown ──▶ Closed
//!   │            ▲               │                             │  ▲
//!   │            │               │ EscalationFatal             │  │ PromotionApproved
//!   ├────────────┼───────────────┴────────────────────────────┘  └──▶ Failed
//!   │ OperatorClose (any live state)          EscalationFatal (Draft/Active/WindingDown)
//!   ▼
//! Closed ◀──OperatorClose── Failed
//! ```
//!
//! The table is total: every `(state, event)` pair not listed is a typed
//! [`CommissionError::IllegalTransition`], never a panic and never a silent
//! no-op. `Created` is constructor-only (the charter op that inserts the
//! record) and is not an addressable event.
//!
//! # Purity
//!
//! Everything here is side-effect free: [`transition`] is a pure function,
//! [`CommissionRecord`] methods mutate only themselves, and
//! [`IndentureDoc::parse`] reads only its input bytes. The daemon is the sole
//! governor that applies events through the charter aggregate.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::sanitize::{sanitize_name, SafeName, SanitizeError};

/// Local result alias: commission ops fail with [`CommissionError`].
type Result<T> = std::result::Result<T, CommissionError>;

// ---------------------------------------------------------------------------
// States and events
// ---------------------------------------------------------------------------

/// The five commission states (ADR-0036 cl. 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CommissionState {
    /// Created but not yet decomposed; no indenture posted.
    #[serde(rename = "draft")]
    Draft,
    /// Indenture posted; roster executing.
    #[serde(rename = "active")]
    Active,
    /// All entries done; converging on the deliverable.
    #[serde(rename = "winding_down")]
    WindingDown,
    /// Converged (promotion approved or closed without one). Terminal.
    #[serde(rename = "closed")]
    Closed,
    /// Escalation fatal, or promotion rejected in review.
    #[serde(rename = "failed")]
    Failed,
}

impl CommissionState {
    /// True while the commission is still in flight (Draft, Active,
    /// WindingDown). Used by freight ACL policy to decide which slots are
    /// shareable grantees (ADR-0036 cl. 2 replaces the legacy `Open` check).
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Draft | Self::Active | Self::WindingDown)
    }
}

/// Per-agent indenture entry status (ADR-0036 cl. 1; SPEC §9.2).
///
/// Forward-only: an entry may only move `Pending → Delivered → InProgress →
/// Done`, never backwards. The enum order *is* the progression order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IndentureEntryStatus {
    /// Rostered; work not yet started.
    #[serde(rename = "pending")]
    Pending,
    /// The agent delivered its slice to the commission.
    #[serde(rename = "delivered")]
    Delivered,
    /// The Mayor is reworking / integrating the delivery.
    #[serde(rename = "in_progress")]
    InProgress,
    /// Complete; counts toward `AllEntriesDone`.
    #[serde(rename = "done")]
    Done,
}

/// Addressable commission events (ADR-0036 cl. 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommissionEvent {
    /// The Mayor posted the INDENTURE.md decomposition: the entry set becomes
    /// `Pending` and the commission starts executing.
    IndenturePosted {
        /// Roster uids that received an indenture entry (non-empty, ⊆ roster).
        entries: BTreeSet<u32>,
    },
    /// Every indenture entry is `Done`; the commission converges.
    AllEntriesDone,
    /// The converged deliverable was submitted for review.
    DeliverableSubmitted,
    /// The operator (or Mayor on operator approval) accepted the promotion.
    PromotionApproved,
    /// The promotion was rejected; the commission fails.
    PromotionRejected,
    /// Operator closed the commission (any live state, and `Failed`).
    OperatorClose,
    /// An escalation reached fatal severity.
    EscalationFatal,
}

impl CommissionEvent {
    /// Stable name for errors and traces.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::IndenturePosted { .. } => "IndenturePosted",
            Self::AllEntriesDone => "AllEntriesDone",
            Self::DeliverableSubmitted => "DeliverableSubmitted",
            Self::PromotionApproved => "PromotionApproved",
            Self::PromotionRejected => "PromotionRejected",
            Self::OperatorClose => "OperatorClose",
            Self::EscalationFatal => "EscalationFatal",
        }
    }
}

/// A `(state, event)` pair that is not in the transition table (ADR-0036).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTransition {
    /// The state the event was applied in.
    pub state: CommissionState,
    /// The event that was refused.
    pub event: &'static str,
}

impl std::fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal commission transition: {} + {}",
            self.state.kind_name(),
            self.event
        )
    }
}

impl CommissionState {
    /// Lowercase wire form (also used in error messages).
    #[must_use]
    pub fn kind_name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::WindingDown => "winding_down",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

/// The total transition table (ADR-0036 cl. 1). Pure: no payload effects, no
/// side channels — record-level preconditions (entries ⊆ roster, all entries
/// done, deliverable in review) are checked by [`CommissionRecord`] methods
/// before the state moves.
///
/// # Errors
///
/// [`IllegalTransition`] for every `(state, event)` pair not in the table.
pub fn transition(state: CommissionState, event: &CommissionEvent) -> Result<CommissionState> {
    let next = match (state, event) {
        (CommissionState::Draft, CommissionEvent::IndenturePosted { .. }) => {
            CommissionState::Active
        }
        (CommissionState::Draft, CommissionEvent::OperatorClose) => CommissionState::Closed,
        (CommissionState::Draft, CommissionEvent::EscalationFatal) => CommissionState::Failed,

        (CommissionState::Active, CommissionEvent::AllEntriesDone) => CommissionState::WindingDown,
        (CommissionState::Active, CommissionEvent::OperatorClose) => CommissionState::Closed,
        (CommissionState::Active, CommissionEvent::EscalationFatal) => CommissionState::Failed,

        (CommissionState::WindingDown, CommissionEvent::DeliverableSubmitted) => {
            CommissionState::WindingDown
        }
        (CommissionState::WindingDown, CommissionEvent::PromotionApproved) => {
            CommissionState::Closed
        }
        (CommissionState::WindingDown, CommissionEvent::PromotionRejected) => {
            CommissionState::Failed
        }
        (CommissionState::WindingDown, CommissionEvent::OperatorClose) => CommissionState::Closed,
        (CommissionState::WindingDown, CommissionEvent::EscalationFatal) => CommissionState::Failed,

        (CommissionState::Failed, CommissionEvent::OperatorClose) => CommissionState::Closed,

        // Every other pair is illegal; the catch-all makes totality explicit.
        _ => {
            return Err(CommissionError::IllegalTransition(IllegalTransition {
                state,
                event: event.kind_name(),
            }))
        }
    };
    Ok(next)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Refusals from commission lifecycle operations (ADR-0036).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommissionError {
    /// `(state, event)` pair not in the transition table.
    IllegalTransition(IllegalTransition),
    /// `IndenturePosted` with an empty entry set.
    EmptyIndenture,
    /// An indenture entry for a uid that is not rostered.
    EntryNotRostered {
        /// The offending uid.
        uid: u32,
    },
    /// `AllEntriesDone` while at least one entry is not `Done`.
    EntriesNotComplete,
    /// A promotion event arrived without a submitted deliverable.
    DeliverableNotInReview,
    /// Roster mutation outside `Draft`/`Active` (the team is fixed once the
    /// commission winds down).
    RosterFrozen,
    /// The uid has no indenture entry to mark.
    UnknownEntry {
        /// The offending uid.
        uid: u32,
    },
    /// Entry status moved backwards (or repeated).
    NotForwardMoving {
        /// The offending uid.
        uid: u32,
        /// Where the entry currently is.
        current: IndentureEntryStatus,
        /// What was requested.
        requested: IndentureEntryStatus,
    },
}

impl std::fmt::Display for CommissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IllegalTransition(t) => write!(f, "{t}"),
            Self::EmptyIndenture => write!(f, "indenture posted with an empty entry set"),
            Self::EntryNotRostered { uid } => {
                write!(f, "indenture entry for uid {uid} is not in the roster")
            }
            Self::EntriesNotComplete => write!(f, "all-entries-done while entries are incomplete"),
            Self::DeliverableNotInReview => {
                write!(f, "promotion event without a submitted deliverable")
            }
            Self::RosterFrozen => write!(f, "roster is frozen outside draft/active"),
            Self::UnknownEntry { uid } => write!(f, "uid {uid} has no indenture entry"),
            Self::NotForwardMoving {
                uid,
                current,
                requested,
            } => write!(
                f,
                "entry {uid}: refusing to move {current_kind:?} backwards to {requested_kind:?}",
                current_kind = current.as_str(),
                requested_kind = requested.as_str()
            ),
        }
    }
}

impl std::error::Error for CommissionError {}

impl IndentureEntryStatus {
    /// Lowercase wire form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }
}

// ---------------------------------------------------------------------------
// The charter record
// ---------------------------------------------------------------------------

/// One commission's state in the charter (ADR-0036 cl. 2).
///
/// The repo path is deliberately absent: it derives from the ledger id via
/// `tree::commission_git(n)` (typed paths; a stored copy would be a second
/// source of truth). Wire form: a strict object (`deny_unknown_fields`);
/// legacy v3 status strings are only legal inside charter.json's migration
/// loader, which maps them before this type ever sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommissionRecord {
    /// Operator-chosen name (sanitized); unique across commissions.
    pub slug: SafeName,
    /// Current state of the machine.
    pub state: CommissionState,
    /// Rostered slot uids (the team). Managed by assign/release ops.
    #[serde(default)]
    pub roster: BTreeSet<u32>,
    /// Indenture entries: uid → status; populated at `IndenturePosted`.
    #[serde(default)]
    pub indenture_entries: BTreeMap<u32, IndentureEntryStatus>,
    /// True once the deliverable is submitted for review (winding down).
    #[serde(default)]
    pub deliverable_in_review: bool,
}

impl CommissionRecord {
    /// Constructor-only creation in `Draft` (the `Created` event of the
    /// state machine; see module docs). Used by `create_commission` and by
    /// legacy auto-registration.
    #[must_use]
    pub fn draft(slug: SafeName) -> Self {
        Self {
            slug,
            state: CommissionState::Draft,
            roster: BTreeSet::new(),
            indenture_entries: BTreeMap::new(),
            deliverable_in_review: false,
        }
    }

    /// The v3 → v4 migration image (ADR-0036 cl. 2): the legacy `open` status
    /// maps to `Draft` (conservative — decomposition is a recorded fact), and
    /// `closed` maps bijectively to `Closed`. The slug is deterministic so
    /// migrated records never collide with operator slugs of other ids.
    #[must_use]
    pub fn migrated(ledger_id: u64, legacy_closed: bool) -> Self {
        let mut record = Self::draft(sanitize_name(&format!("commission-{ledger_id}")).unwrap());
        if legacy_closed {
            record.state = CommissionState::Closed;
        }
        record
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> CommissionState {
        self.state
    }

    /// Whether the roster may still change (draft/active).
    #[must_use]
    pub fn roster_open(&self) -> bool {
        matches!(self.state, CommissionState::Draft | CommissionState::Active)
    }

    // --- state-machine events --------------------------------------------

    /// Apply an addressable event with its record-level preconditions. The
    /// only way a record's `state` changes after construction.
    ///
    /// # Errors
    ///
    /// [`CommissionError`] — see the variant docs; every refusal is typed.
    pub fn apply_event(&mut self, event: &CommissionEvent) -> Result<()> {
        match event {
            CommissionEvent::IndenturePosted { entries } => {
                if entries.is_empty() {
                    return Err(CommissionError::EmptyIndenture);
                }
                for uid in entries.iter() {
                    if !self.roster.contains(uid) {
                        return Err(CommissionError::EntryNotRostered { uid: *uid });
                    }
                }
                self.state = transition(self.state, event)?;
                for uid in entries.iter() {
                    self.indenture_entries
                        .insert(*uid, IndentureEntryStatus::Pending);
                }
            }
            CommissionEvent::AllEntriesDone => {
                if !self.all_entries_done() {
                    return Err(CommissionError::EntriesNotComplete);
                }
                self.state = transition(self.state, event)?;
            }
            CommissionEvent::DeliverableSubmitted => {
                self.state = transition(self.state, event)?;
                self.deliverable_in_review = true;
            }
            CommissionEvent::PromotionApproved | CommissionEvent::PromotionRejected => {
                if !self.deliverable_in_review {
                    return Err(CommissionError::DeliverableNotInReview);
                }
                self.state = transition(self.state, event)?;
                self.deliverable_in_review = false;
            }
            CommissionEvent::OperatorClose | CommissionEvent::EscalationFatal => {
                self.state = transition(self.state, event)?;
            }
        }
        Ok(())
    }

    /// Whether every indenture entry is `Done` (the `AllEntriesDone`
    /// precondition).
    #[must_use]
    pub fn all_entries_done(&self) -> bool {
        !self.indenture_entries.is_empty()
            && self
                .indenture_entries
                .values()
                .all(|s| *s == IndentureEntryStatus::Done)
    }

    // --- roster ------------------------------------------------------------

    /// Add a uid to the roster (draft/active only).
    ///
    /// # Errors
    ///
    /// [`CommissionError::RosterFrozen`] outside draft/active.
    pub fn assign_uid(&mut self, uid: u32) -> Result<()> {
        if !self.roster_open() {
            return Err(CommissionError::RosterFrozen);
        }
        self.roster.insert(uid);
        Ok(())
    }

    /// Remove a uid from the roster (draft/active only).
    ///
    /// # Errors
    ///
    /// [`CommissionError::RosterFrozen`] outside draft/active.
    pub fn release_uid(&mut self, uid: u32) -> Result<()> {
        if !self.roster_open() {
            return Err(CommissionError::RosterFrozen);
        }
        self.roster.remove(&uid);
        Ok(())
    }

    // --- indenture entry status -------------------------------------------

    /// Move one indenture entry forward (active only; the machine that marks
    /// work is the executing commission).
    ///
    /// # Errors
    ///
    /// [`CommissionError`] — unknown entry, backwards move, or wrong state.
    pub fn mark_entry(&mut self, uid: u32, status: IndentureEntryStatus) -> Result<()> {
        if self.state != CommissionState::Active {
            return Err(CommissionError::IllegalTransition(IllegalTransition {
                state: self.state,
                event: "MarkIndentureEntry",
            }));
        }
        let current = self
            .indenture_entries
            .get(&uid)
            .ok_or(CommissionError::UnknownEntry { uid })?;
        if !status_ord(status).gt(&status_ord(*current)) {
            return Err(CommissionError::NotForwardMoving {
                uid,
                current: *current,
                requested: status,
            });
        }
        self.indenture_entries.insert(uid, status);
        Ok(())
    }
}

fn status_ord(status: IndentureEntryStatus) -> u8 {
    match status {
        IndentureEntryStatus::Pending => 0,
        IndentureEntryStatus::Delivered => 1,
        IndentureEntryStatus::InProgress => 2,
        IndentureEntryStatus::Done => 3,
    }
}

// ---------------------------------------------------------------------------
// INDENTURE.md grammar (SPEC §9.2, ADR-0036 cl. 3)
// ---------------------------------------------------------------------------

/// Maximum INDENTURE.md size accepted by the parser (ADR-0036 cl. 3).
pub const MAX_INDENTURE_BYTES: usize = 32 * 1024;

/// Maximum length of a single indenture field value in bytes.
pub const MAX_INDENTURE_FIELD_BYTES: usize = 512;

/// One agent's indenture block (SPEC §9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndentureBlock {
    /// The roster identity this entry belongs to.
    pub agent: SafeName,
    /// What the agent must achieve.
    pub goal: String,
    /// Which files/areas the agent owns.
    pub scope: String,
    /// The observable condition that counts as done.
    pub done: String,
}

/// A parsed INDENTURE.md (SPEC §9.2, ADR-0036 cl. 3).
///
/// Grammar (strict; LF-only):
/// - line 1: `# INDENTURE`
/// - blocks: `## <agent>` (SafeName charset), each with exactly three bullets
///   in order — `- goal:`, `- scope:`, `- done:` — one non-empty value each
/// - blank lines are allowed between blocks; nothing else is legal
/// - at least one block; the whole file ≤ [`MAX_INDENTURE_BYTES`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndentureDoc {
    /// Entries keyed by agent (sorted on render).
    pub entries: BTreeMap<SafeName, IndentureBlock>,
}

/// Parse failures with the offending line (1-based) where applicable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndentureError {
    /// The input exceeds [`MAX_INDENTURE_BYTES`].
    TooLarge {
        /// Actual byte length.
        len: usize,
    },
    /// CRLF/CR line endings (LF-only at every text boundary).
    BadLineEnding {
        /// 1-based line.
        line: usize,
    },
    /// Line 1 is not `# INDENTURE`.
    MissingHeader {
        /// 1-based line.
        line: usize,
    },
    /// A heading or bullet the grammar does not recognize.
    UnexpectedLine {
        /// 1-based line.
        line: usize,
        /// What was found (truncated to 64 chars for stable messages).
        text: String,
    },
    /// An agent name that fails sanitization.
    BadAgentName {
        /// 1-based line.
        line: usize,
        /// The offending token.
        value: String,
        /// Why it is not a SafeName.
        source: SanitizeError,
    },
    /// The same agent appears in two blocks.
    DuplicateAgent {
        /// 1-based line of the second occurrence.
        line: usize,
        /// The agent name.
        agent: String,
    },
    /// A field value exceeding [`MAX_INDENTURE_FIELD_BYTES`].
    FieldTooLong {
        /// 1-based line.
        line: usize,
        /// The field (goal/scope/done).
        field: &'static str,
    },
    /// An empty field value.
    EmptyField {
        /// 1-based line.
        line: usize,
        /// The field (goal/scope/done).
        field: &'static str,
    },
    /// No indenture blocks at all.
    NoEntries,
}

impl std::fmt::Display for IndentureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len } => {
                write!(
                    f,
                    "INDENTURE.md is {len} bytes; cap is {MAX_INDENTURE_BYTES}"
                )
            }
            Self::BadLineEnding { line } => write!(f, "line {line}: CRLF/CR not allowed (LF only)"),
            Self::MissingHeader { line } => {
                write!(f, "line {line}: expected `# INDENTURE` as the first line")
            }
            Self::UnexpectedLine { line, text } => {
                write!(
                    f,
                    "line {line}: not part of the indenture grammar: `{text}`"
                )
            }
            Self::BadAgentName {
                line,
                value,
                source,
            } => {
                write!(f, "line {line}: agent name `{value}` is invalid: {source}")
            }
            Self::DuplicateAgent { line, agent } => {
                write!(
                    f,
                    "line {line}: agent `{agent}` already has an indenture block"
                )
            }
            Self::FieldTooLong { line, field } => {
                write!(
                    f,
                    "line {line}: {field} exceeds {MAX_INDENTURE_FIELD_BYTES} bytes"
                )
            }
            Self::EmptyField { line, field } => write!(f, "line {line}: {field} is empty"),
            Self::NoEntries => write!(f, "INDENTURE.md has no agent blocks"),
        }
    }
}

impl std::error::Error for IndentureError {}

impl IndentureDoc {
    /// Parse an INDENTURE.md document (strict grammar; see type docs).
    ///
    /// # Errors
    ///
    /// [`IndentureError`] with the offending line.
    pub fn parse(text: &str) -> std::result::Result<Self, IndentureError> {
        if text.len() > MAX_INDENTURE_BYTES {
            return Err(IndentureError::TooLarge { len: text.len() });
        }

        let mut entries: BTreeMap<SafeName, IndentureBlock> = BTreeMap::new();
        let mut current_agent: Option<SafeName> = None;
        // Which of the three bullets we are inside a block (0 = none yet).
        let mut field_index: usize = 0;

        for (idx, line) in text.split('\n').enumerate() {
            let line_no = idx + 1;
            if line.ends_with('\r') {
                return Err(IndentureError::BadLineEnding { line: line_no });
            }

            if idx == 0 {
                if line != "# INDENTURE" {
                    return Err(IndentureError::MissingHeader { line: line_no });
                }
                continue;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // blank lines between blocks are legal
            }

            if let Some(rest) = trimmed.strip_prefix("## ") {
                // New block: the previous one (if any) must be complete.
                if current_agent.is_some() && field_index != 3 {
                    return Err(IndentureError::UnexpectedLine {
                        line: line_no,
                        text: truncate(trimmed),
                    });
                }
                let raw = rest.trim();
                let agent = sanitize_name(raw).map_err(|source| IndentureError::BadAgentName {
                    line: line_no,
                    value: raw.to_owned(),
                    source,
                })?;
                if entries.contains_key(&agent) {
                    return Err(IndentureError::DuplicateAgent {
                        line: line_no,
                        agent: agent.as_str().to_owned(),
                    });
                }
                current_agent = Some(agent);
                field_index = 0;
                continue;
            }

            let Some(agent) = &current_agent else {
                return Err(IndentureError::UnexpectedLine {
                    line: line_no,
                    text: truncate(trimmed),
                });
            };

            // Exactly three bullets, in order.
            let field = ["goal", "scope", "done"].get(field_index).copied();
            let Some(expected) = field else {
                return Err(IndentureError::UnexpectedLine {
                    line: line_no,
                    text: truncate(trimmed),
                });
            };
            let Some(value) = trimmed.strip_prefix(&format!("- {expected}:")) else {
                return Err(IndentureError::UnexpectedLine {
                    line: line_no,
                    text: truncate(trimmed),
                });
            };
            let value = value.trim();
            if value.is_empty() {
                return Err(IndentureError::EmptyField {
                    line: line_no,
                    field: expected,
                });
            }
            if value.len() > MAX_INDENTURE_FIELD_BYTES {
                return Err(IndentureError::FieldTooLong {
                    line: line_no,
                    field: expected,
                });
            }

            let block = entries
                .entry(agent.clone())
                .or_insert_with(|| IndentureBlock {
                    agent: agent.clone(),
                    goal: String::new(),
                    scope: String::new(),
                    done: String::new(),
                });
            match expected {
                "goal" => block.goal = value.to_owned(),
                "scope" => block.scope = value.to_owned(),
                _ => block.done = value.to_owned(),
            }
            field_index += 1;
        }

        // Final block must have all three fields.
        if current_agent.is_some() && field_index != 3 {
            return Err(IndentureError::UnexpectedLine {
                line: text.split('\n').count(),
                text: "end of file".into(),
            });
        }
        if entries.is_empty() {
            return Err(IndentureError::NoEntries);
        }
        Ok(Self { entries })
    }

    /// Canonical rendering (sorted by agent name). `parse(render(doc)) == doc`
    /// for every parsed document.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("# INDENTURE\n");
        for block in self.entries.values() {
            out.push_str(&format!(
                "\n## {}\n- goal: {}\n- scope: {}\n- done: {}\n",
                block.agent, block.goal, block.scope, block.done
            ));
        }
        out
    }
}

fn truncate(text: &str) -> String {
    text.chars().take(64).collect()
}

// ---------------------------------------------------------------------------
// INDENTURE.md.ack + completion notice helpers (SPEC §9.2–§9.3)
// ---------------------------------------------------------------------------

/// Current `INDENTURE.md.ack` schema version.
pub const INDENTURE_ACK_VERSION: u32 = 1;

/// Outcome recorded in `guild/ledger/commission<n>/INDENTURE.md.ack`
/// (SPEC §9.2; ADR-0026 cl. 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndentureAckStatus {
    /// `INDENTURE.md` validated and all entries delivered to rostered agents.
    Done,
    /// `INDENTURE.md` failed schema or roster validation; Mayor re-woken.
    Rejected,
    /// Validation retries exhausted; commission transitioned to `Failed` and
    /// an escalation was filed under `keep/census/manor/escalations/`.
    Escalated,
}

impl IndentureAckStatus {
    /// Wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Rejected => "rejected",
            Self::Escalated => "escalated",
        }
    }
}

impl std::fmt::Display for IndentureAckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// On-disk acknowledgment for `guild/ledger/commission<n>/INDENTURE.md`
/// (`INDENTURE.md.ack`).
///
/// Field declaration order is alphabetical so `serde_json` emits sorted keys
/// (CONVENTIONS §3 r1), and no wall-clock field is carried (CONVENTIONS §3 r4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndentureAck {
    /// Always [`INDENTURE_ACK_VERSION`].
    pub ack_version: u32,
    /// Consecutive validation failures observed on `commission<n>` (0 when
    /// `status == Done`).
    pub attempt: u32,
    /// Ledger index (`commission<n>`, n ≥ 1).
    pub commission: u64,
    /// FNV-1a 64-bit hex digest of the evaluated `INDENTURE.md` bytes so
    /// passive rescans are idempotent on unchanged files.
    pub digest: String,
    /// Human-readable validation error when `status != Done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Validation outcome.
    pub status: IndentureAckStatus,
}

impl IndentureAck {
    /// Construct a `done` acknowledgment for a validated `INDENTURE.md`.
    #[must_use]
    pub fn done(commission: u64, digest: impl Into<String>) -> Self {
        Self {
            ack_version: INDENTURE_ACK_VERSION,
            attempt: 0,
            commission,
            digest: digest.into(),
            error: None,
            status: IndentureAckStatus::Done,
        }
    }

    /// Construct a `rejected` acknowledgment for a failed validation attempt.
    #[must_use]
    pub fn rejected(
        commission: u64,
        attempt: u32,
        digest: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            ack_version: INDENTURE_ACK_VERSION,
            attempt,
            commission,
            digest: digest.into(),
            error: Some(error.into()),
            status: IndentureAckStatus::Rejected,
        }
    }

    /// Construct an `escalated` acknowledgment when validation retries are
    /// exhausted.
    #[must_use]
    pub fn escalated(
        commission: u64,
        attempt: u32,
        digest: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            ack_version: INDENTURE_ACK_VERSION,
            attempt,
            commission,
            digest: digest.into(),
            error: Some(error.into()),
            status: IndentureAckStatus::Escalated,
        }
    }

    /// Canonical pretty-printed JSON with trailing `\n`.
    ///
    /// # Panics
    ///
    /// Never — all fields are primitive/string types whose serialization is
    /// infallible.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s =
            serde_json::to_string_pretty(self).expect("IndentureAck serialization is infallible");
        s.push('\n');
        s
    }

    /// Parse an `INDENTURE.md.ack` JSON payload.
    ///
    /// # Errors
    ///
    /// Returns the underlying `serde_json::Error` if the JSON is malformed or
    /// carries an unsupported `ack_version`.
    pub fn parse(text: &str) -> std::result::Result<Self, serde_json::Error> {
        let ack: Self = serde_json::from_str(text)?;
        if ack.ack_version != INDENTURE_ACK_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported ack_version {} (expected {INDENTURE_ACK_VERSION})",
                ack.ack_version
            )));
        }
        Ok(ack)
    }
}

/// Parse an agent-to-Mayor indenture entry status notice mail `Subject` line
/// (SPEC §9.3).
///
/// Recognized canonical syntax (case-insensitive, surrounding whitespace
/// ignored):
/// - `commission:<n>:done` / `commission<n>:done` → `(n, IndentureEntryStatus::Done)`
/// - `commission:<n>:in_progress` / `commission<n>:in_progress` → `(n, IndentureEntryStatus::InProgress)`
/// - `commission:<n>:delivered` / `commission<n>:delivered` → `(n, IndentureEntryStatus::Delivered)`
///
/// An optional trailing free-text description after whitespace (e.g.
/// `commission:1:done — tests green`) is permitted.
#[must_use]
pub fn parse_entry_notice_subject(subject: &str) -> Option<(u64, IndentureEntryStatus)> {
    let token = subject.split_whitespace().next()?;
    let lower = token.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("commission:")
        .or_else(|| lower.strip_prefix("commission"))?;
    let (num_str, status_str) = rest.split_once(':')?;
    let n: u64 = num_str.parse().ok()?;
    if n == 0 {
        return None;
    }
    let status = match status_str {
        "done" | "complete" | "completed" => IndentureEntryStatus::Done,
        "in_progress" | "in-progress" | "working" => IndentureEntryStatus::InProgress,
        "delivered" => IndentureEntryStatus::Delivered,
        _ => return None,
    };
    Some((n, status))
}

/// Parse an agent-authored deliverable submission subject line
/// (e.g. `Subject: commission:1:deliverable` or `Subject: commission:1:submit`).
///
/// An optional trailing free-text description after whitespace (e.g.
/// `commission:1:deliverable — ready for promotion`) or subpath (e.g. `commission:1:deliverable:path`)
/// is permitted.
#[must_use]
pub fn parse_deliverable_notice_subject(subject: &str) -> Option<(u64, Option<String>)> {
    let token = subject.split_whitespace().next()?;
    let lower = token.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("commission:")
        .or_else(|| lower.strip_prefix("commission"))?;
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.is_empty() {
        return None;
    }
    let n: u64 = parts[0].parse().ok()?;
    if n == 0 {
        return None;
    }
    if parts.len() < 2 {
        return None;
    }
    let action = parts[1];
    if action != "deliverable" && action != "submit" && action != "submission" {
        return None;
    }
    let subpath = if parts.len() > 2 {
        let p = parts[2..].join(":");
        if p.is_empty() {
            None
        } else {
            Some(p)
        }
    } else {
        None
    };
    Some((n, subpath))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> SafeName {
        sanitize_name(s).unwrap()
    }

    fn record_with_roster() -> CommissionRecord {
        let mut r = CommissionRecord::draft(name("ledger"));
        r.roster.insert(1001);
        r.roster.insert(1002);
        r
    }

    // --- transition table ---------------------------------------------------

    #[test]
    fn full_happy_path() {
        let mut r = record_with_roster();
        assert_eq!(r.state(), CommissionState::Draft);
        r.apply_event(&CommissionEvent::IndenturePosted {
            entries: r.roster.clone(),
        })
        .unwrap();
        assert_eq!(r.state(), CommissionState::Active);

        for uid in [1001, 1002] {
            r.mark_entry(uid, IndentureEntryStatus::Delivered).unwrap();
            r.mark_entry(uid, IndentureEntryStatus::Done).unwrap();
        }
        r.apply_event(&CommissionEvent::AllEntriesDone).unwrap();
        assert_eq!(r.state(), CommissionState::WindingDown);

        r.apply_event(&CommissionEvent::DeliverableSubmitted)
            .unwrap();
        assert!(r.deliverable_in_review);
        r.apply_event(&CommissionEvent::PromotionApproved).unwrap();
        assert_eq!(r.state(), CommissionState::Closed);
    }

    #[test]
    fn every_illegal_pair_is_typed() {
        // Draft cannot skip decomposition.
        assert!(matches!(
            transition(CommissionState::Draft, &CommissionEvent::AllEntriesDone),
            Err(CommissionError::IllegalTransition(_))
        ));
        // Closed is terminal to everything.
        for event in [
            CommissionEvent::IndenturePosted {
                entries: BTreeSet::new(),
            },
            CommissionEvent::AllEntriesDone,
            CommissionEvent::DeliverableSubmitted,
            CommissionEvent::PromotionApproved,
            CommissionEvent::PromotionRejected,
            CommissionEvent::OperatorClose,
            CommissionEvent::EscalationFatal,
        ] {
            assert!(
                transition(CommissionState::Closed, &event).is_err(),
                "Closed must reject {}",
                event.kind_name()
            );
        }
        // Failed only closes.
        assert_eq!(
            transition(CommissionState::Failed, &CommissionEvent::OperatorClose).unwrap(),
            CommissionState::Closed
        );
        assert!(transition(
            CommissionState::Failed,
            &CommissionEvent::IndenturePosted {
                entries: BTreeSet::new()
            }
        )
        .is_err());
    }

    #[test]
    fn indenture_preconditions() {
        let mut r = record_with_roster();
        // Empty entry set.
        assert!(matches!(
            r.apply_event(&CommissionEvent::IndenturePosted {
                entries: BTreeSet::new()
            }),
            Err(CommissionError::EmptyIndenture)
        ));
        // Entry outside the roster.
        let mut bad = r.roster.clone();
        bad.insert(1999);
        assert!(matches!(
            r.apply_event(&CommissionEvent::IndenturePosted { entries: bad }),
            Err(CommissionError::EntryNotRostered { uid: 1999 })
        ));
    }

    #[test]
    fn entry_status_is_forward_only() {
        let mut r = record_with_roster();
        r.apply_event(&CommissionEvent::IndenturePosted {
            entries: r.roster.clone(),
        })
        .unwrap();
        // Pending -> Done is legal (skips are forward).
        r.mark_entry(1001, IndentureEntryStatus::Done).unwrap();
        // Backwards and repeats are refused.
        assert!(matches!(
            r.mark_entry(1001, IndentureEntryStatus::Delivered),
            Err(CommissionError::NotForwardMoving { .. })
        ));
        assert!(matches!(
            r.mark_entry(1001, IndentureEntryStatus::Done),
            Err(CommissionError::NotForwardMoving { .. })
        ));
        // Unknown entry.
        assert!(matches!(
            r.mark_entry(1999, IndentureEntryStatus::Done),
            Err(CommissionError::UnknownEntry { uid: 1999 })
        ));
    }

    #[test]
    fn roster_freezes_at_winding_down() {
        let mut r = record_with_roster();
        r.apply_event(&CommissionEvent::IndenturePosted {
            entries: r.roster.clone(),
        })
        .unwrap();
        for uid in [1001, 1002] {
            r.mark_entry(uid, IndentureEntryStatus::Done).unwrap();
        }
        r.apply_event(&CommissionEvent::AllEntriesDone).unwrap();
        assert!(matches!(
            r.assign_uid(1003),
            Err(CommissionError::RosterFrozen)
        ));
        assert!(matches!(
            r.release_uid(1001),
            Err(CommissionError::RosterFrozen)
        ));
    }

    #[test]
    fn promotion_requires_review() {
        let mut r = record_with_roster();
        r.apply_event(&CommissionEvent::IndenturePosted {
            entries: r.roster.clone(),
        })
        .unwrap();
        for uid in [1001, 1002] {
            r.mark_entry(uid, IndentureEntryStatus::Done).unwrap();
        }
        r.apply_event(&CommissionEvent::AllEntriesDone).unwrap();
        assert!(matches!(
            r.apply_event(&CommissionEvent::PromotionApproved),
            Err(CommissionError::DeliverableNotInReview)
        ));
        r.apply_event(&CommissionEvent::DeliverableSubmitted)
            .unwrap();
        r.apply_event(&CommissionEvent::PromotionRejected).unwrap();
        assert_eq!(r.state(), CommissionState::Failed);
    }

    // --- INDENTURE parsing ---------------------------------------------------

    const DOC: &str = "# INDENTURE\n\
        \n\
        ## ada\n\
        - goal: ship the ledger export\n\
        - scope: crates/ledger/**\n\
        - done: `cargo test` green and export verified\n\
        \n\
        ## grace\n\
        - goal: write the operator guide\n\
        - scope: docs/\n\
        - done: guide reviewed\n";

    #[test]
    fn parses_valid_doc() {
        let doc = IndentureDoc::parse(DOC).unwrap();
        assert_eq!(doc.entries.len(), 2);
        let ada = &doc.entries[&name("ada")];
        assert_eq!(ada.goal, "ship the ledger export");
        assert_eq!(ada.scope, "crates/ledger/**");
        assert_eq!(ada.done, "`cargo test` green and export verified");
    }

    #[test]
    fn render_round_trips() {
        let doc = IndentureDoc::parse(DOC).unwrap();
        let rendered = doc.render();
        let again = IndentureDoc::parse(&rendered).unwrap();
        assert_eq!(doc, again);
    }

    #[test]
    fn header_is_mandatory() {
        let err = IndentureDoc::parse("## ada\n- goal: x\n").unwrap_err();
        assert!(matches!(err, IndentureError::MissingHeader { line: 1 }));
    }

    #[test]
    fn crlf_is_refused_with_line() {
        let err = IndentureDoc::parse("# INDENTURE\r\n## a\n- goal: x\n").unwrap_err();
        assert!(matches!(err, IndentureError::BadLineEnding { line: 1 }));
    }

    #[test]
    fn duplicate_agent_is_refused_with_line() {
        let text = "# INDENTURE\n## a\n- goal: x\n- scope: y\n- done: z\n## a\n- goal: x\n";
        let err = IndentureDoc::parse(text).unwrap_err();
        assert!(matches!(
            err,
            IndentureError::DuplicateAgent { line: 6, .. }
        ));
    }

    #[test]
    fn missing_field_is_refused() {
        let text = "# INDENTURE\n## a\n- goal: x\n- scope: y\n";
        let err = IndentureDoc::parse(text).unwrap_err();
        assert!(matches!(err, IndentureError::UnexpectedLine { .. }));
    }

    #[test]
    fn empty_field_is_refused() {
        let text = "# INDENTURE\n## a\n- goal:\n- scope: y\n- done: z\n";
        let err = IndentureDoc::parse(text).unwrap_err();
        assert!(matches!(
            err,
            IndentureError::EmptyField { field: "goal", .. }
        ));
    }

    #[test]
    fn no_entries_is_refused() {
        let err = IndentureDoc::parse("# INDENTURE\n").unwrap_err();
        assert!(matches!(err, IndentureError::NoEntries));
    }

    #[test]
    fn oversized_doc_is_refused() {
        let big = format!(
            "# INDENTURE\n## a\n- goal: {}\n- scope: y\n- done: z\n",
            "x".repeat(MAX_INDENTURE_BYTES)
        );
        assert!(matches!(
            IndentureDoc::parse(&big),
            Err(IndentureError::TooLarge { .. })
        ));
    }

    #[test]
    fn bad_agent_name_is_refused() {
        let err = IndentureDoc::parse("# INDENTURE\n## ../evil\n- goal: x\n").unwrap_err();
        assert!(matches!(err, IndentureError::BadAgentName { .. }));
    }

    // --- serde ----------------------------------------------------------------

    #[test]
    fn record_serde_round_trip() {
        let mut r = record_with_roster();
        r.apply_event(&CommissionEvent::IndenturePosted {
            entries: r.roster.clone(),
        })
        .unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: CommissionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn record_rejects_unknown_fields() {
        let err = serde_json::from_str::<CommissionRecord>(
            r#"{"slug":"a","state":"draft","roster":[],"indenture_entries":{},"deliverable_in_review":false,"bogus":1}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn indenture_ack_round_trip() {
        for ack in [
            IndentureAck::done(1, "0123456789abcdef"),
            IndentureAck::rejected(2, 1, "fedcba9876543210", "line 1: missing header"),
            IndentureAck::escalated(3, 3, "deadbeefcafebabe", "retries exhausted"),
        ] {
            let rendered = ack.render();
            assert!(rendered.ends_with('\n'));
            let parsed = IndentureAck::parse(&rendered).unwrap();
            assert_eq!(parsed, ack);
        }
    }

    #[test]
    fn entry_notice_subject_parser() {
        assert_eq!(
            parse_entry_notice_subject("commission:1:done"),
            Some((1, IndentureEntryStatus::Done))
        );
        assert_eq!(
            parse_entry_notice_subject("commission1:done — tests green"),
            Some((1, IndentureEntryStatus::Done))
        );
        assert_eq!(
            parse_entry_notice_subject("COMMISSION:12:in_progress"),
            Some((12, IndentureEntryStatus::InProgress))
        );
        assert_eq!(
            parse_entry_notice_subject("commission:2:delivered"),
            Some((2, IndentureEntryStatus::Delivered))
        );
        assert_eq!(parse_entry_notice_subject("commission:0:done"), None);
        assert_eq!(parse_entry_notice_subject("commission:1:bogus"), None);
        assert_eq!(parse_entry_notice_subject("Normal status update"), None);
    }

    #[test]
    fn deliverable_notice_subject_parser() {
        assert_eq!(
            parse_deliverable_notice_subject("commission:1:deliverable"),
            Some((1, None))
        );
        assert_eq!(
            parse_deliverable_notice_subject("commission1:deliverable:final.tar.gz — ready"),
            Some((1, Some("final.tar.gz".to_owned())))
        );
        assert_eq!(
            parse_deliverable_notice_subject("COMMISSION:12:submit"),
            Some((12, None))
        );
        assert_eq!(
            parse_deliverable_notice_subject("commission:0:deliverable"),
            None
        );
        assert_eq!(parse_deliverable_notice_subject("commission:1:other"), None);
        assert_eq!(
            parse_deliverable_notice_subject("Regular message to mayor"),
            None
        );
    }
}
