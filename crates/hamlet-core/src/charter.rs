//! Charter state model + atomic canonical serde (SPEC §11.1; CONVENTIONS §3;
//! ADR-0004).
//!
//! In-memory state over `BTreeMap`/`BTreeSet` — iteration order is sorted,
//! always. Canonical human-readable JSON with sorted keys (serde_json over
//! `BTreeMap`), mandatory `charter_version: u32`, additive-only migrations
//! with explicit version steps. No wall-clock reads anywhere in state
//! derivation (timestamps are telemetry, never state inputs).
//!
//! **Normative content groups** (SPEC §11.1; later sprints extend
//! additively): [`slots`](crate::slots) (uid → identity record), `agents`
//! (registry refs), `commissions` (id → [`CommissionRecord`](crate::commission::CommissionRecord),
//! charter v4, ADR-0036), `parcels` (uid
//! lease records), `sessions` (uid → supervised session record, charter v2,
//! ADR-0015), `decrees` (decree sequence → queue status) + `decree_receipts`
//! (decree sequence → admitted content digest, charter v3, ADR-0026),
//! `webhooks` (config ref names).
//!
//! **Persistence protocol** (CONVENTIONS §3 rule 2; ADR-0006):
//! [`Charter::save_atomic`] serializes to a *unique* temp file in the same
//! directory (`.<name>.<pid>.<serial>.tmp` — concurrent savers never share
//! a temp) → write → fsync → set canonical mode 0644 → rename → fsync
//! parent dir. Never in place; the legacy deterministic temp name
//! ([`Charter::temp_path`]) is swept before each save. Together
//! with [`HamletTree::ensure`](crate::tree::HamletTree::ensure) this is the
//! second (and only other) sanctioned I/O point in hamlet-core (ADR-0004,
//! extending the ADR-0003 precedent): the atomic write protocol is itself
//! the unit under test, so it cannot be meaningfully mocked.
//!
//! [`Charter::load`] treats corruption as a hard error carrying the offending
//! byte offset — never silent recovery.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::commission::{CommissionEvent, CommissionRecord, IndentureEntryStatus};
use crate::sanitize::SafeName;
use crate::session::{SessionId, SessionRecord};
use crate::slots::{self, SlotError, SlotTable, FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};

/// Current charter schema version. Mandatory field (CONVENTIONS §3 rule 3);
/// migrations are additive-only with explicit version steps (ADR-0004).
///
/// v1 → v2 (Sprint 03, ADR-0015): additive `sessions` group — supervised
/// session identity/status/restart counter per UID slot. v1 files still
/// load: the migration step stamps the version; serde defaults fill the
/// group. v2 → v3 (Sprint 05, ADR-0026 cl. 3): additive
/// `decree_receipts` group — the content digest each admitted decree id was
/// recorded with, so replaying an id with different content is *refused*
/// rather than silently dropped. `decrees` (id → status) is untouched, so
/// v2 files still load. v3 → v4 (Sprint 06, ADR-0036 cl. 2): the
/// `commissions` value type retypes from the 2-state placeholder string to
/// [`CommissionRecord`](crate::commission::CommissionRecord); the legacy
/// string carries exactly two bits and maps bijectively at parse time
/// (`open` → draft, `closed` → closed), so v3 files still load.
pub const CHARTER_VERSION: u32 = 4;

/// Master state map — the typed form of `.hamlet/charter.json`.
///
/// All groups are `BTreeMap`/`BTreeSet` (sorted keys fall out naturally) and
/// serialize away when empty, so a fresh village renders exactly
/// `{"charter_version": 2}`. Parsing is strict (`deny_unknown_fields` via
/// the [`StrictCharter`] wire mirror): unknown fields are corruption at
/// every supported version, older-version documents simply omit newer
/// groups (additive migration fills defaults), and newer files are rejected
/// by the version check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charter {
    /// Schema version; see [`CHARTER_VERSION`].
    pub charter_version: u32,

    /// UID slot table: uid → permanent identity record (SPEC §4). 1000 is
    /// the Mayor; 1001–2000 are sequential permanent subagent slots.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: SlotTable,

    /// Agent registry refs (`guild/register/`): name → registry binding.
    /// v1 placeholder shape; sprint 03 (agent engine) extends additively.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<SafeName, AgentRef>,

    /// Commission states: ledger id → lifecycle record (SPEC §9; ADR-0036).
    /// The state machine, roster, indenture entries, and review flag live in
    /// the record; the repo path derives from the ledger id via
    /// `tree::commission_git(n)` and is deliberately not stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commissions: BTreeMap<u64, CommissionRecord>,

    /// Parcel lease records (`keep/census/parcel<uid>/`): uid → lease.
    /// `stasis` marks a patron whose home is parked under
    /// `tavern/patrons/` (SPEC §10); the slot mapping persists regardless.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parcels: BTreeMap<u32, ParcelLease>,

    /// Supervised session records (SPEC §6; ADR-0015): uid → live-or-last
    /// session identity, status, and consecutive-restart counter. Added in
    /// charter v2; Sprint 03 supervision owns the transitions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<u32, SessionRecord>,

    /// Decree queue status: decree sequence → lifecycle status. The sequence
    /// is the monotonic ordering key assigned by [`Charter::next_decree_seq`]
    /// (ADR-0026 cl. 3) — never a clock reading.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub decrees: BTreeMap<u64, DecreeStatus>,

    /// Admitted decree receipts: decree sequence → the content digest it was
    /// admitted with (ADR-0026 cl. 3, charter v3). The *idempotent replay
    /// record*: a repeated decree with the same digest is a no-op, the same
    /// sequence with a different digest is refused. Keys mirror `decrees`
    /// exactly (load-time invariant).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub decree_receipts: BTreeMap<u64, String>,

    /// Webhook config refs: sanitized names pointing at webhook config
    /// (delivery wiring lands in a later sprint). Placeholder shape.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub webhooks: BTreeSet<SafeName>,
}

/// Agent registry reference (v1 placeholder — sprint 03 extends).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRef {
    /// The permanent UID slot this registered agent identity runs under.
    pub slot: u32,
}

/// Parcel lease record (SPEC §10 census tenant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParcelLease {
    /// Identity holding the lease; must equal the slot record's identity.
    pub identity: SafeName,
    /// True while the patron's home is parked in stasis
    /// (`tavern/patrons/PROPRIETOR<name>/home.uid.bak/`).
    pub stasis: bool,
}

/// Outcome of [`Charter::admit_decree`] (ADR-0026 cl. 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecreeAdmission {
    /// First sighting: receipt + queued status recorded; the executor runs.
    Admitted,
    /// Already executed with identical content: idempotent no-op.
    Replayed,
}

/// Decree queue status placeholder (sprint 05 extends).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecreeStatus {
    /// Awaiting daemon processing.
    Queued,
    /// Executed successfully.
    Delivered,
    /// Execution failed; surfaced to the Mayor/operator.
    Failed,
}

/// Hard errors from charter load/save. Corruption is never silently
/// recovered (task contract): [`CharterError::Corrupt`] carries line,
/// column, and the offending **byte offset** into the file.
#[derive(Debug)]
pub enum CharterError {
    /// Filesystem failure with the step that failed.
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// Malformed JSON / unknown field / schema-shape violation.
    Corrupt {
        /// 1-based line of the offending position.
        line: usize,
        /// 1-based column of the offending position.
        column: usize,
        /// 0-based byte offset of the offending position in the file.
        byte_offset: usize,
        /// serde classification/detail.
        detail: String,
    },
    /// `charter_version` outside the supported range (future or zero).
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Highest version this build supports.
        supported: u32,
    },
    /// Structurally valid JSON that violates a charter invariant
    /// (cross-group consistency, uid ranges).
    Invariant {
        /// What is inconsistent.
        message: String,
    },
    /// A commission lifecycle operation refused by the state machine
    /// (ADR-0036) — typed at [`crate::commission::CommissionError`], carried
    /// here as a message to keep one error channel for charter ops.
    Commission {
        /// What was refused and why.
        message: String,
    },
}

impl fmt::Display for CharterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Corrupt {
                line,
                column,
                byte_offset,
                detail,
            } => write!(
                f,
                "charter.json corrupt at byte {byte_offset} (line {line}, column {column}): {detail}"
            ),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "charter_version {found} is not supported by this build (max {supported}); refusing to guess"
            ),
            Self::Invariant { message } => write!(f, "charter invariant violated: {message}"),
            Self::Commission { message } => {
                write!(f, "commission operation refused: {message}")
            }
        }
    }
}

impl std::error::Error for CharterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<crate::commission::CommissionError> for CharterError {
    fn from(error: crate::commission::CommissionError) -> Self {
        Self::Commission {
            message: error.to_string(),
        }
    }
}

/// Charter `Result` alias.
pub type Result<T> = std::result::Result<T, CharterError>;

fn io_err(context: &str) -> impl FnOnce(std::io::Error) -> CharterError + '_ {
    move |source| CharterError::Io {
        context: context.to_string(),
        source,
    }
}

impl Default for Charter {
    fn default() -> Self {
        Self::empty()
    }
}

impl Charter {
    /// The empty versioned skeleton `hamlet init` writes: all normative
    /// groups present in memory, serialized away while empty.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            charter_version: CHARTER_VERSION,
            slots: BTreeMap::new(),
            agents: BTreeMap::new(),
            commissions: BTreeMap::new(),
            parcels: BTreeMap::new(),
            sessions: BTreeMap::new(),
            decrees: BTreeMap::new(),
            decree_receipts: BTreeMap::new(),
            webhooks: BTreeSet::new(),
        }
    }

    /// Canonical human-readable rendering: sorted keys *everywhere* — the
    /// state serializes to a [`serde_json::Value`] tree (objects are
    /// `BTreeMap`s, so both group keys and record keys sort regardless of
    /// struct declaration order), then pretty-prints. Trailing newline.
    /// Deterministic — byte-identical for equal state, always.
    #[must_use]
    pub fn render(&self) -> String {
        let value =
            serde_json::to_value(self).expect("BTreeMap-based Charter serialization is infallible");
        let mut out =
            serde_json::to_string_pretty(&value).expect("Value pretty-printing is infallible");
        out.push('\n');
        out
    }

    /// Legacy deterministic temp path `.<file_name>.tmp` in the same
    /// directory. Since ADR-0006 live saves write to a *unique* temp per
    /// attempt ([`Charter::save_atomic`]); this name remains the canonical
    /// stale-temp sweep target (crash leftovers from pre-ADR-0006 writers)
    /// and the squat-detection canary: a save hard-fails when it cannot be
    /// swept (e.g. a directory squatting the name).
    ///
    /// # Panics
    ///
    /// Panics only if `path` has no parent or no file name.
    #[must_use]
    pub fn temp_path(path: &Path) -> PathBuf {
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(path.file_name().expect("charter path needs a file name"));
        tmp_name.push(".tmp");
        path.parent()
            .expect("charter path needs a parent directory")
            .join(tmp_name)
    }

    /// Load the charter from `path`: parse → version gate → migration chain
    /// → invariant validation. Corruption is a hard error carrying the
    /// offending byte offset; nothing is silently repaired.
    ///
    /// # Errors
    ///
    /// [`CharterError::Io`], [`CharterError::Corrupt`],
    /// [`CharterError::UnsupportedVersion`], or [`CharterError::Invariant`].
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(io_err(&format!("read {}", path.display())))?;
        Self::from_bytes(&bytes)
    }

    /// Load from raw bytes (the byte-offset reporting core of
    /// [`Charter::load`]; also the entry point for golden-vector tests).
    ///
    /// # Errors
    ///
    /// Same as [`Charter::load`], minus I/O.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        // Parsing is strict (deny_unknown_fields) against the UNION of
        // supported schemas: an older-version document simply *omits* newer
        // groups (serde defaults fill them — additive migration,
        // CONVENTIONS §3 r3), while unknown fields are corruption at any
        // supported version.
        let charter = match serde_json::from_slice::<StrictCharter>(bytes).map(Charter::from) {
            Ok(charter) => charter,
            Err(err) => {
                // A clean Value parse with a too-new version is a version
                // error, not corruption — report it precisely.
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
                    if let Some(found) = value.get("charter_version").and_then(|v| v.as_u64()) {
                        if found > u64::from(CHARTER_VERSION) {
                            return Err(CharterError::UnsupportedVersion {
                                found: u32::try_from(found).unwrap_or(u32::MAX),
                                supported: CHARTER_VERSION,
                            });
                        }
                    }
                }
                return Err(corrupt(bytes, &err));
            }
        };
        gate_version(charter.charter_version)?;
        migrate(charter)
    }

    /// Atomically persist to `path`: sweep the legacy temp → render into a
    /// UNIQUE same-directory temp (ADR-0006: concurrent savers never share
    /// a temp file) → write → fsync → chmod 0644 → rename → fsync parent
    /// dir. Never writes in place; on any failure the temp file is removed
    /// best-effort and a previously committed charter survives untouched.
    /// An un-sweepable legacy temp (e.g. a directory squatting the name) is
    /// a hard error raised BEFORE the committed charter is touched.
    ///
    /// # Errors
    ///
    /// [`CharterError::Io`] naming the failing step.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        let bytes = self.render();
        let parent = path
            .parent()
            .ok_or_else(|| CharterError::Io {
                context: format!("save {}: no parent directory", path.display()),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"),
            })?
            .to_path_buf();

        // Sweep the legacy deterministic temp first (ADR-0006): stale crash
        // leftovers are consumed by the next save, and anything that cannot
        // be swept (e.g. a directory squatting the name) fails hard before
        // the committed charter is touched.
        let legacy_tmp = Self::temp_path(path);
        if let Err(source) = fs::remove_file(&legacy_tmp) {
            if source.kind() != std::io::ErrorKind::NotFound {
                return Err(CharterError::Io {
                    context: format!(
                        "save {}: sweep stale temp {}",
                        path.display(),
                        legacy_tmp.display()
                    ),
                    source,
                });
            }
        }

        let tmp = unique_temp_path(path);
        let result = (|| -> std::io::Result<()> {
            {
                let mut file = fs::File::create(&tmp)?;
                file.write_all(bytes.as_bytes())?;
                file.sync_all()?;
            }
            set_canonical_mode(&tmp)?;
            fs::rename(&tmp, path)?;
            // fsync the directory so the rename itself is durable.
            if let Ok(dir) = fs::File::open(&parent) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();

        result.map_err(|source| {
            let _ = fs::remove_file(&tmp); // best-effort cleanup
            CharterError::Io {
                context: format!("save {}", path.display()),
                source,
            }
        })
    }

    // --- decree admission (ADR-0026 cl. 3–4) ---------------------------

    /// The next decree sequence to use (1-based): one past the highest
    /// admitted sequence, or `1` for a fresh village. This is the *only*
    /// ordering source for decrees — no wall-clock, no mtime, no filename
    /// timestamp ever participates (CONVENTIONS §3 r4).
    #[must_use]
    pub fn next_decree_seq(&self) -> u64 {
        self.decrees.keys().next_back().map_or(1, |seq| seq + 1)
    }

    /// Admit a decree id (ADR-0026 cl. 3–4), the idempotent gate every
    /// execution passes through:
    ///
    /// - **unknown sequence** → it must be exactly [`Charter::next_decree_seq`];
    ///   the receipt and the [`DecreeStatus::Queued`] status are recorded in
    ///   one mutation, so a crash between the two is impossible (the next
    ///   save is atomic, ADR-0006).
    /// - **known sequence, same digest** → [`DecreeAdmission::Replayed`]: an
    ///   idempotent no-op (the daemon re-acknowledges `done`).
    /// - **known sequence, different digest** → `Err`: the sequence was
    ///   reused for different content, which the daemon acks as
    ///   `rejected: replay_mismatch` rather than executing.
    ///
    /// # Errors
    ///
    /// [`CharterError::Invariant`] naming which rule refused the id.
    pub fn admit_decree(
        &mut self,
        id: &crate::decree::DecreeId,
    ) -> std::result::Result<DecreeAdmission, CharterError> {
        match self.decree_receipts.get(&id.seq()) {
            Some(recorded) if recorded == id.digest() => Ok(DecreeAdmission::Replayed),
            Some(recorded) => Err(invariant(format!(
                "decree sequence {} was already admitted with a different content digest \
                 (recorded {recorded}, offered {}) — refusing to reuse a decree id \
                 (ADR-0026 cl. 3)",
                id.seq(),
                id.digest()
            ))),
            None => {
                let expected = self.next_decree_seq();
                if id.seq() != expected {
                    return Err(invariant(format!(
                        "decree sequence {} is not the next charter sequence {expected} — \
                         ordering is the charter counter, never the clock \
                         (CONVENTIONS §3 r4)",
                        id.seq()
                    )));
                }
                self.decree_receipts
                    .insert(id.seq(), id.digest().to_owned());
                self.decrees.insert(id.seq(), DecreeStatus::Queued);
                Ok(DecreeAdmission::Admitted)
            }
        }
    }

    /// Record the terminal status of an admitted decree.
    ///
    /// # Errors
    ///
    /// [`CharterError::Invariant`] when `seq` was never admitted (its receipt
    /// must exist).
    pub fn set_decree_status(
        &mut self,
        seq: u64,
        status: DecreeStatus,
    ) -> std::result::Result<(), CharterError> {
        if !self.decree_receipts.contains_key(&seq) {
            return Err(invariant(format!(
                "decree sequence {seq} has no admission receipt (status cannot precede \
                 admission, ADR-0026 cl. 3)"
            )));
        }
        self.decrees.insert(seq, status);
        Ok(())
    }

    // --- slot operations (delegate to crate::slots over self.slots) ---

    /// Reserve UID 1000 for the Mayor/manor identity (idempotent).
    ///
    /// # Errors
    ///
    /// [`SlotError::MayorConflict`] if the slot belongs to another identity.
    pub fn reserve_mayor(&mut self, identity: SafeName) -> std::result::Result<u32, SlotError> {
        slots::reserve_mayor(&mut self.slots, identity)
    }

    /// Allocate the next sequential permanent subagent slot (1001–2000).
    /// Idempotent per identity; hibernated/crashed slots keep their mapping
    /// and are never reused.
    ///
    /// # Errors
    ///
    /// [`SlotError::Exhausted`] — a typed hard error, never a wrap.
    pub fn allocate_slot(
        &mut self,
        identity: SafeName,
        commission: Option<u64>,
    ) -> std::result::Result<u32, SlotError> {
        slots::allocate(&mut self.slots, identity, commission)
    }

    // --- commission lifecycle (SPEC §9; ADR-0036) -------------------------

    /// The next commission ledger id: 1-based, `max(existing) + 1`.
    #[must_use]
    pub fn next_commission_id(&self) -> u64 {
        self.commissions.keys().max().map_or(1, |m| m + 1)
    }

    /// Create a new commission in `Draft` (SPEC §9.1). The slug must be
    /// unique across commissions; the ledger id is allocated by this
    /// aggregate.
    ///
    /// # Errors
    ///
    /// [`CharterError::Commission`] when the slug is already taken.
    pub fn create_commission(&mut self, slug: SafeName) -> Result<u64> {
        let taken = self
            .commissions
            .iter()
            .find(|(_, record)| record.slug == slug);
        if let Some((n, _)) = taken {
            return Err(CharterError::Commission {
                message: format!("slug '{}' already used by commission {n}", slug.as_str()),
            });
        }
        let n = self.next_commission_id();
        self.commissions.insert(n, CommissionRecord::draft(slug));
        Ok(n)
    }

    /// Legacy auto-registration (ADR-0036 cl. 2): idempotently ensure a
    /// `Draft` record exists for ledger id `n`, slugged `commission-{n}`.
    /// A present record — in any state — is left untouched.
    ///
    /// # Errors
    ///
    /// [`CharterError::Commission`] for non-1-based ids.
    pub fn ensure_commission(&mut self, n: u64) -> Result<()> {
        if n < 1 {
            return Err(CharterError::Commission {
                message: "commission ids are 1-based".into(),
            });
        }
        self.commissions
            .entry(n)
            .or_insert_with(|| CommissionRecord::migrated(n, false));
        Ok(())
    }

    /// Roster a slot uid to commission `n` (draft/active only), mirroring
    /// the binding into the slot table.
    ///
    /// # Errors
    ///
    /// [`CharterError::Commission`] — unknown commission, missing slot,
    /// slot already bound elsewhere, or roster frozen.
    pub fn assign_uid(&mut self, n: u64, uid: u32) -> Result<()> {
        let Some(slot) = self.slots.get(&uid) else {
            return Err(CharterError::Commission {
                message: format!("uid {uid} has no slot record"),
            });
        };
        if let Some(other) = slot.commission {
            if other != n {
                return Err(CharterError::Commission {
                    message: format!("slot {uid} already serves commission {other}"),
                });
            }
        }
        let record = self
            .commissions
            .get_mut(&n)
            .ok_or_else(|| CharterError::Commission {
                message: format!("commission {n} does not exist"),
            })?;
        record.assign_uid(uid).map_err(commission_refusal)?;
        if let Some(slot) = self.slots.get_mut(&uid) {
            slot.commission = Some(n);
        }
        Ok(())
    }

    /// Unroster a slot uid from commission `n` (draft/active only), clearing
    /// the slot's commission binding.
    ///
    /// # Errors
    ///
    /// [`CharterError::Commission`] — unknown commission, uid not rostered,
    /// or roster frozen.
    pub fn release_uid(&mut self, n: u64, uid: u32) -> Result<()> {
        let record = self
            .commissions
            .get_mut(&n)
            .ok_or_else(|| CharterError::Commission {
                message: format!("commission {n} does not exist"),
            })?;
        if !record.roster.contains(&uid) {
            return Err(CharterError::Commission {
                message: format!("uid {uid} is not rostered to commission {n}"),
            });
        }
        record.release_uid(uid).map_err(commission_refusal)?;
        if let Some(slot) = self.slots.get_mut(&uid) {
            slot.commission = None;
        }
        Ok(())
    }

    /// Apply an addressable commission event (the typed transition table,
    /// ADR-0036 cl. 1). The only path a commission's state changes after
    /// creation.
    ///
    /// # Errors
    ///
    /// [`CharterError::Commission`] — unknown commission or refused
    /// `(state, event)` pair / precondition.
    pub fn apply_commission_event(&mut self, n: u64, event: &CommissionEvent) -> Result<()> {
        let record = self
            .commissions
            .get_mut(&n)
            .ok_or_else(|| CharterError::Commission {
                message: format!("commission {n} does not exist"),
            })?;
        record.apply_event(event).map_err(commission_refusal)
    }

    /// Move one indenture entry forward (active only; ADR-0036 cl. 1).
    ///
    /// # Errors
    ///
    /// [`CharterError::Commission`] — unknown commission, unknown entry,
    /// backwards move, or wrong state.
    pub fn mark_indenture_entry(
        &mut self,
        n: u64,
        uid: u32,
        status: IndentureEntryStatus,
    ) -> Result<()> {
        let record = self
            .commissions
            .get_mut(&n)
            .ok_or_else(|| CharterError::Commission {
                message: format!("commission {n} does not exist"),
            })?;
        record.mark_entry(uid, status).map_err(commission_refusal)
    }

    /// Validate cross-group invariants (ADR-0004). Run on every load; also
    /// available for daemon-side sanity checks after mutation batches.
    ///
    /// # Errors
    ///
    /// [`CharterError::Invariant`] naming the first violation (sorted order).
    pub fn validate(&self) -> Result<()> {
        if self.charter_version == 0 || self.charter_version > CHARTER_VERSION {
            return Err(CharterError::UnsupportedVersion {
                found: self.charter_version,
                supported: CHARTER_VERSION,
            });
        }
        let mut bound_identities = BTreeSet::new();
        for (&uid, record) in &self.slots {
            let in_range = uid == MAYOR_UID || (FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid);
            if !in_range {
                return Err(invariant(format!(
                    "slot uid {uid} outside {MAYOR_UID} and {FIRST_AGENT_UID}-{LAST_AGENT_UID}"
                )));
            }
            // One identity holds exactly ONE permanent slot (SPEC §4;
            // ADR-0008) — duplicates make allocate/hibernate ambiguous.
            if !bound_identities.insert(&record.identity) {
                return Err(invariant(format!(
                    "slot identity '{identity}' bound to multiple uids (again at {uid})",
                    identity = record.identity
                )));
            }
            if let Some(commission) = record.commission {
                if !self.commissions.contains_key(&commission) {
                    return Err(invariant(format!(
                        "slot {uid} references unknown commission {commission}"
                    )));
                }
            }
        }
        // Roster implies binding (ADR-0036 cl. 2): every rostered uid must
        // hold a slot record bound to this commission.
        for (&n, record) in &self.commissions {
            if n < 1 {
                return Err(invariant(format!("commission id {n} is not 1-based")));
            }
            for uid in &record.roster {
                match self.slots.get(uid) {
                    Some(slot) if slot.commission == Some(n) => {}
                    Some(slot) => {
                        return Err(invariant(format!(
                            "commission {n} rosters uid {uid} but its slot serves commission {:?}",
                            slot.commission
                        )));
                    }
                    None => {
                        return Err(invariant(format!(
                            "commission {n} rosters missing slot {uid}"
                        )));
                    }
                }
            }
        }
        for (name, agent) in &self.agents {
            match self.slots.get(&agent.slot) {
                Some(record) if record.identity == *name => {}
                Some(_) => {
                    return Err(invariant(format!(
                        "agent '{name}' bound to slot {} held by another identity",
                        agent.slot
                    )));
                }
                None => {
                    return Err(invariant(format!(
                        "agent '{name}' references missing slot {}",
                        agent.slot
                    )));
                }
            }
        }
        for (&uid, lease) in &self.parcels {
            match self.slots.get(&uid) {
                Some(record) if record.identity == lease.identity => {}
                Some(_) => {
                    return Err(invariant(format!(
                        "parcel {uid} leased to '{}' but slot belongs to '{}'",
                        lease.identity, self.slots[&uid].identity
                    )));
                }
                None => {
                    return Err(invariant(format!(
                        "parcel {uid} has no slot record (slots are permanent; this is corruption)"
                    )));
                }
            }
        }
        for (&uid, session) in &self.sessions {
            if session.id.uid != uid {
                return Err(invariant(format!(
                    "sessions key {uid} does not match session uid {}",
                    session.id.uid
                )));
            }
            if SessionId::new(uid, session.id.incarnation).is_err() {
                return Err(invariant(format!(
                    "session uid {uid} outside {MAYOR_UID} and {FIRST_AGENT_UID}-{LAST_AGENT_UID}"
                )));
            }
            if !self.slots.contains_key(&uid) {
                return Err(invariant(format!(
                    "session for uid {uid} has no slot record (slots are permanent; this is corruption)"
                )));
            }
        }
        for (&seq, digest) in &self.decree_receipts {
            if seq < 1 {
                return Err(invariant(format!(
                    "decree sequence {seq} is not a 1-based sequence"
                )));
            }
            if digest.len() != crate::decree::DIGEST_HEX_LEN
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invariant(format!(
                    "decree receipt for sequence {seq} is not a {} hex-char digest",
                    crate::decree::DIGEST_HEX_LEN
                )));
            }
            if !self.decrees.contains_key(&seq) {
                return Err(invariant(format!(
                    "decree receipt {seq} has no matching decrees entry (receipts and \
                     status records must mirror each other)"
                )));
            }
        }
        for &seq in self.decrees.keys() {
            if !self.decree_receipts.contains_key(&seq) {
                return Err(invariant(format!(
                    "decree status {seq} has no admission receipt (status without a receipt \
                     means an id was executed without being admitted)"
                )));
            }
        }
        Ok(())
    }
}

fn invariant(message: String) -> CharterError {
    CharterError::Invariant { message }
}

/// Carry a typed commission refusal (ADR-0036) through the charter error
/// channel; the machine's own vocabulary is preserved in the message.
fn commission_refusal(error: crate::commission::CommissionError) -> CharterError {
    CharterError::Commission {
        message: error.to_string(),
    }
}

/// Version gate: reject zero and anything from the future before migration.
fn gate_version(found: u32) -> Result<()> {
    if found == 0 || found > CHARTER_VERSION {
        return Err(CharterError::UnsupportedVersion {
            found,
            supported: CHARTER_VERSION,
        });
    }
    Ok(())
}

/// Additive-only migration chain with explicit version steps (CONVENTIONS
/// §3 rule 3). Each step upgrades `v → v+1` by *adding* fields/groups with
/// defaults — never renaming, retyping, or dropping state. Today the chain
/// is a single version; this skeleton is the committed shape every future
/// step plugs into:
///
/// ```ignore
/// if current.charter_version == 1 { current = step_1_to_2(current)?; }
/// ```
fn migrate(charter: Charter) -> Result<Charter> {
    let mut current = charter;
    if current.charter_version == 1 {
        current = step_1_to_2(current)?;
    }
    if current.charter_version == 2 {
        current = step_2_to_3(current)?;
    }
    if current.charter_version == 3 {
        current = step_3_to_4(current)?;
    }
    if current.charter_version != CHARTER_VERSION {
        return Err(invariant(format!(
            "no migration step from charter_version {} to {CHARTER_VERSION}",
            current.charter_version
        )));
    }
    current.validate()?;
    Ok(current)
}

/// v1 → v2 (Sprint 03, ADR-0015): additive `sessions` group. Serde
/// defaults already populated the group at parse time; the step only stamps
/// the new version — never renames, retypes, or drops state.
fn step_1_to_2(mut charter: Charter) -> Result<Charter> {
    charter.charter_version = 2;
    Ok(charter)
}

/// v2 → v3 (Sprint 05, ADR-0026 cl. 3): additive `decree_receipts` group.
/// Serde defaults already populated the group at parse time; the step only
/// stamps the new version — never renames, retypes, or drops state.
fn step_2_to_3(mut charter: Charter) -> Result<Charter> {
    charter.charter_version = 3;
    Ok(charter)
}

/// v3 → v4 (Sprint 06, ADR-0036 cl. 2): the `commissions` value retype.
/// The wire layer already mapped legacy status strings to records at parse
/// time (bijective: `open` → draft, `closed` → closed); the step only
/// stamps the version — never renames, retypes, or drops state a second time.
fn step_3_to_4(mut charter: Charter) -> Result<Charter> {
    charter.charter_version = 4;
    Ok(charter)
}

/// The two legacy v3 commission statuses, parse-time validated (ADR-0036
/// cl. 2): any other string is corruption at every version.
#[derive(Deserialize)]
enum LegacyCommissionStatus {
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "closed")]
    Closed,
}

/// Wire form for one commission value (ADR-0036 cl. 2): the current record
/// object, or the legacy v3 status string. Implemented via a direct visitor
/// rather than `#[serde(untagged)]` so `serde_json`'s map-key coercion for
/// `indenture_entries: BTreeMap<u32, IndentureEntryStatus>` is preserved.
enum CommissionWire {
    /// Legacy v3 status (bijective migration source).
    Legacy(LegacyCommissionStatus),
    /// Current record object (charter v4+).
    Record(CommissionRecord),
}

impl<'de> Deserialize<'de> for CommissionWire {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CommissionWireVisitor;

        impl<'de> serde::de::Visitor<'de> for CommissionWireVisitor {
            type Value = CommissionWire;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a commission record object or legacy status string (\"open\" | \"closed\")",
                )
            }

            fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match v {
                    "open" => Ok(CommissionWire::Legacy(LegacyCommissionStatus::Open)),
                    "closed" => Ok(CommissionWire::Legacy(LegacyCommissionStatus::Closed)),
                    other => Err(E::unknown_variant(other, &["open", "closed"])),
                }
            }

            fn visit_map<M>(self, map: M) -> std::result::Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let record = CommissionRecord::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(CommissionWire::Record(record))
            }
        }

        deserializer.deserialize_any(CommissionWireVisitor)
    }
}

/// Unknown-fields-strict wire mirror of [`Charter`] (v4 schema). Public
/// parsing is lenient *only* for documents declaring an older version —
/// additive migration (CONVENTIONS §3 r3) means a v1 file simply lacks the
/// newer groups; it must never mean new fields are silently dropped from a
/// current-version file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictCharter {
    charter_version: u32,
    #[serde(default)]
    slots: SlotTable,
    #[serde(default)]
    agents: BTreeMap<SafeName, AgentRef>,
    #[serde(default)]
    commissions: BTreeMap<u64, CommissionWire>,
    #[serde(default)]
    parcels: BTreeMap<u32, ParcelLease>,
    #[serde(default)]
    sessions: BTreeMap<u32, SessionRecord>,
    #[serde(default)]
    decrees: BTreeMap<u64, DecreeStatus>,
    #[serde(default)]
    decree_receipts: BTreeMap<u64, String>,
    #[serde(default)]
    webhooks: BTreeSet<SafeName>,
}

impl From<StrictCharter> for Charter {
    fn from(wire: StrictCharter) -> Self {
        // Legacy v3 status strings map bijectively to records (ADR-0036
        // cl. 2): `open` → draft (decomposition is a recorded fact),
        // `closed` → closed, with the deterministic migrated slug.
        let commissions: BTreeMap<u64, CommissionRecord> = wire
            .commissions
            .into_iter()
            .map(|(n, wire_value)| {
                let record = match wire_value {
                    CommissionWire::Legacy(status) => CommissionRecord::migrated(
                        n,
                        matches!(status, LegacyCommissionStatus::Closed),
                    ),
                    CommissionWire::Record(record) => record,
                };
                (n, record)
            })
            .collect();
        Self {
            charter_version: wire.charter_version,
            slots: wire.slots,
            agents: wire.agents,
            commissions,
            parcels: wire.parcels,
            sessions: wire.sessions,
            decrees: wire.decrees,
            decree_receipts: wire.decree_receipts,
            webhooks: wire.webhooks,
        }
    }
}

/// Build a [`CharterError::Corrupt`] with the offending byte offset derived
/// from serde's 1-based line/column over the raw bytes.
fn corrupt(bytes: &[u8], err: &serde_json::Error) -> CharterError {
    let (line, column) = (err.line(), err.column());
    let mut offset = 0usize;
    let mut byte_offset = bytes.len();
    let chunks = bytes.split_inclusive(|&b| b == b'\n');
    for (current_line, chunk) in (1usize..).zip(chunks) {
        if current_line == line {
            let text = String::from_utf8_lossy(chunk);
            let within = text
                .char_indices()
                .nth(column.saturating_sub(1))
                .map_or(text.len(), |(i, _)| i);
            byte_offset = offset + within;
            break;
        }
        offset += chunk.len();
    }
    CharterError::Corrupt {
        line,
        column,
        byte_offset,
        detail: err.to_string(),
    }
}

/// Unique temp path for one save attempt (ADR-0006):
/// `.<file_name>.<pid>.<serial>.tmp` in the same directory (same
/// filesystem, so the final `rename` is atomic). The pid+serial pair
/// guarantees no two live attempts — across threads or processes — ever
/// share a temp file, so a rename can never publish a peer's half-written
/// bytes.
///
/// # Panics
///
/// Panics only if `path` has no parent or no file name.
fn unique_temp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let serial = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(path.file_name().expect("charter path needs a file name"));
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), serial));
    path.parent()
        .expect("charter path needs a parent directory")
        .join(tmp_name)
}

/// Canonical 0644 for charter.json (golden-file contract, deterministic
/// under any umask).
#[cfg(unix)]
fn set_canonical_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn set_canonical_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::sanitize_name;
    use crate::session::SessionStatus;
    use crate::slots::{SlotRecord, SlotStatus, AGENT_SLOT_CAPACITY};

    fn name(s: &str) -> SafeName {
        sanitize_name(s).unwrap()
    }

    /// A populated but small charter exercising every normative group.
    fn sample_charter() -> Charter {
        let mut charter = Charter::empty();
        charter.reserve_mayor(name("manor")).unwrap();
        let ada = charter.allocate_slot(name("ada"), Some(1)).unwrap();
        charter.allocate_slot(name("grace"), None).unwrap();
        charter
            .commissions
            .insert(1, CommissionRecord::draft(name("ledger")));
        charter.agents.insert(name("ada"), AgentRef { slot: ada });
        charter.parcels.insert(
            ada,
            ParcelLease {
                identity: name("ada"),
                stasis: false,
            },
        );
        charter.decrees.insert(1, DecreeStatus::Queued);
        charter
            .decree_receipts
            .insert(1, "c0225c3b8fabf543".to_string());
        charter.webhooks.insert(name("github"));
        charter.sessions.insert(
            ada,
            SessionRecord {
                id: SessionId::new(ada, 0).unwrap(),
                status: SessionStatus::Idle,
                restart_count: 0,
            },
        );
        charter
    }

    /// Committed golden vector: the exact bytes `hamlet init` writes for a
    /// fresh village (empty groups serialize away).
    #[test]
    fn empty_charter_render_golden() {
        assert_eq!(
            Charter::empty().render(),
            "{\n  \"charter_version\": 4\n}\n"
        );
    }

    #[test]
    fn empty_charter_roundtrips_and_carries_version() {
        let rendered = Charter::empty().render();
        let parsed: Charter = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed, Charter::empty());
        assert_eq!(parsed.charter_version, CHARTER_VERSION);
        assert_eq!(Charter::default(), Charter::empty());
    }

    /// Committed migration vector (ADR-0015 cl. 4 + ADR-0026 cl. 3): a
    /// v1-rendered charter loads as v3 with the empty `sessions` and
    /// `decree_receipts` groups defaulted in; all other state untouched.
    #[test]
    fn v1_charter_migrates_additively() {
        let v1 = br#"{
  "agents": { "ada": { "slot": 1001 } },
  "charter_version": 1,
  "slots": {
    "1000": { "identity": "manor", "status": "active" },
    "1001": { "identity": "ada", "status": "active" }
  }
}"#;
        let mut charter = Charter::from_bytes(v1).unwrap();
        assert_eq!(charter.charter_version, CHARTER_VERSION);
        assert!(charter.sessions.is_empty());
        assert!(charter.decree_receipts.is_empty());
        assert_eq!(charter.slots.len(), 2);
        // The supervisor writes the first session record; save/load
        // round-trips the v3 shape.
        charter.sessions.insert(
            1001,
            SessionRecord {
                id: SessionId::new(1001, 0).unwrap(),
                status: SessionStatus::Starting,
                restart_count: 0,
            },
        );
        let rendered = charter.render();
        assert!(rendered.contains("\"charter_version\": 4"));
        assert_eq!(Charter::from_bytes(rendered.as_bytes()).unwrap(), charter);
    }

    /// v2 → v3 (ADR-0026 cl. 3) is additive: a v2 document that already
    /// carries `decrees` but no `decree_receipts` still parses. Such a
    /// document is only loadable if it carries no `decrees` entries (status
    /// without a receipt is the v2 placeholder shape, which the receipt
    /// invariant refuses) — that refusal is the documented migration edge.
    #[test]
    fn v2_charter_without_decrees_migrates_additively() {
        let v2 = br#"{
  "charter_version": 2,
  "slots": {
    "1000": { "identity": "manor", "status": "active" }
  }
}"#;
        let charter = Charter::from_bytes(v2).unwrap();
        assert_eq!(charter.charter_version, CHARTER_VERSION);
        assert!(charter.decrees.is_empty());
        assert!(charter.decree_receipts.is_empty());
        // A v2 document that already recorded a decree status is refused
        // rather than repaired (no silent state invention, CONVENTIONS §3 r3).
        let v2_with_decrees = br#"{
  "charter_version": 2,
  "decrees": { "1": "queued" }
}"#;
        assert!(matches!(
            Charter::from_bytes(v2_with_decrees),
            Err(CharterError::Invariant { .. })
        ));
    }

    /// ADR-0026 cl. 3–4: the decree admission gate. Sequence ordering comes
    /// from the charter counter (never a clock), an identical re-emission is
    /// an idempotent no-op, and a reused sequence with different content is
    /// refused.
    #[test]
    fn decree_admission_is_monotonic_and_replay_protected() {
        use crate::decree::DecreeId;

        let mut charter = Charter::empty();
        assert_eq!(charter.next_decree_seq(), 1, "1-based sequence");

        let first = DecreeId::new(1, "c0225c3b8fabf543").unwrap();
        assert_eq!(
            charter.admit_decree(&first).unwrap(),
            DecreeAdmission::Admitted
        );
        assert_eq!(charter.decrees[&1], DecreeStatus::Queued);
        assert_eq!(charter.decree_receipts[&1], "c0225c3b8fabf543");
        assert_eq!(charter.next_decree_seq(), 2);

        // Identical re-emission: idempotent no-op, no state change.
        let before = charter.clone();
        assert_eq!(
            charter.admit_decree(&first).unwrap(),
            DecreeAdmission::Replayed
        );
        assert_eq!(charter, before, "a replay must not mutate state");

        // Same sequence, different content: refused (not silently executed).
        let forged = DecreeId::new(1, "0000000000000001").unwrap();
        match charter.admit_decree(&forged) {
            Err(CharterError::Invariant { message }) => {
                assert!(message.contains("already admitted"), "{message}")
            }
            other => panic!("expected replay-mismatch refusal, got {other:?}"),
        }

        // Skipping a sequence is refused: ordering is the charter counter.
        let skipped = DecreeId::new(5, "0000000000000002").unwrap();
        match charter.admit_decree(&skipped) {
            Err(CharterError::Invariant { message }) => {
                assert!(
                    message.contains("not the next charter sequence"),
                    "{message}"
                )
            }
            other => panic!("expected out-of-order refusal, got {other:?}"),
        }

        // Terminal status is only reachable for an admitted sequence.
        charter
            .set_decree_status(1, DecreeStatus::Delivered)
            .unwrap();
        assert_eq!(charter.decrees[&1], DecreeStatus::Delivered);
        assert!(matches!(
            charter.set_decree_status(2, DecreeStatus::Failed),
            Err(CharterError::Invariant { .. })
        ));
        assert!(charter.validate().is_ok());

        // Kill/restart: admission survives the atomic save (CONVENTIONS §3 r6).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("charter.json");
        charter.save_atomic(&path).unwrap();
        let mut recovered = Charter::load(&path).unwrap();
        assert_eq!(recovered.decree_receipts, charter.decree_receipts);
        assert_eq!(
            recovered.admit_decree(&first).unwrap(),
            DecreeAdmission::Replayed,
            "a replayed decree is still idempotent after restart"
        );
        assert!(
            matches!(
                recovered.admit_decree(&forged),
                Err(CharterError::Invariant { .. })
            ),
            "a reused sequence with different content stays refused after restart"
        );
    }

    /// Receipt/status mirroring and digest shape are load-time invariants
    /// (ADR-0026 cl. 3).
    #[test]
    fn decree_receipt_invariants_reject_lying_charters() {
        let bad_digest = br#"{
  "charter_version": 3,
  "decrees": { "1": "queued" },
  "decree_receipts": { "1": "NOT-HEX" }
}"#;
        assert!(matches!(
            Charter::from_bytes(bad_digest),
            Err(CharterError::Invariant { .. })
        ));
        let zero_seq = br#"{
  "charter_version": 3,
  "decrees": { "0": "queued" },
  "decree_receipts": { "0": "c0225c3b8fabf543" }
}"#;
        assert!(matches!(
            Charter::from_bytes(zero_seq),
            Err(CharterError::Invariant { .. })
        ));
        let receipt_only = br#"{
  "charter_version": 3,
  "decree_receipts": { "1": "c0225c3b8fabf543" }
}"#;
        assert!(matches!(
            Charter::from_bytes(receipt_only),
            Err(CharterError::Invariant { .. })
        ));
    }

    /// Unknown fields are corruption at every supported version — union-
    /// strict parsing is not relaxed for older declared versions.
    #[test]
    fn v2_charter_rejects_unknown_fields() {
        let bad = br#"{"charter_version": 2, "surprise": true}"#;
        assert!(matches!(
            Charter::from_bytes(bad),
            Err(CharterError::Corrupt { .. })
        ));
        let bad_session = br#"{
  "charter_version": 2,
  "slots": { "1001": { "identity": "ada", "status": "active" } },
  "sessions": { "1001": { "id": { "uid": 1001, "incarnation": 0 }, "status": "idle", "restart_count": 0, "extra": 1 } }
}"#;
        assert!(matches!(
            Charter::from_bytes(bad_session),
            Err(CharterError::Corrupt { .. })
        ));
    }

    /// Sessions-group invariants: map key == `id.uid`; the slot must exist.
    #[test]
    fn session_invariants_reject_lying_charters() {
        let key_mismatch = br#"{
  "charter_version": 2,
  "slots": {
    "1001": { "identity": "ada", "status": "active" },
    "1002": { "identity": "grace", "status": "active" }
  },
  "sessions": { "1001": { "id": { "uid": 1002, "incarnation": 0 }, "status": "idle", "restart_count": 0 } }
}"#;
        match Charter::from_bytes(key_mismatch) {
            Err(CharterError::Invariant { message }) => {
                assert!(message.contains("does not match session uid"), "{message}")
            }
            other => panic!("expected key/uid Invariant, got {other:?}"),
        }
        let missing_slot = br#"{
  "charter_version": 2,
  "sessions": { "1001": { "id": { "uid": 1001, "incarnation": 0 }, "status": "idle", "restart_count": 0 } }
}"#;
        match Charter::from_bytes(missing_slot) {
            Err(CharterError::Invariant { message }) => {
                assert!(message.contains("has no slot record"), "{message}")
            }
            other => panic!("expected missing-slot Invariant, got {other:?}"),
        }
        // Constructed in memory, validate() must catch the same lie.
        let mut charter = Charter::empty();
        charter.sessions.insert(
            1001,
            SessionRecord {
                id: SessionId::new(1001, 0).unwrap(),
                status: SessionStatus::Crashed,
                restart_count: 5,
            },
        );
        assert!(matches!(
            charter.validate(),
            Err(CharterError::Invariant { .. })
        ));
    }

    /// Committed golden vector: full canonical rendering — groups sorted
    /// alphabetically, keys sorted, snake_case enum tags, trailing newline.
    #[test]
    fn populated_charter_render_golden() {
        let golden = "\
{
  \"agents\": {
    \"ada\": {
      \"slot\": 1001
    }
  },
  \"charter_version\": 4,
  \"commissions\": {
    \"1\": {
      \"deliverable_in_review\": false,
      \"indenture_entries\": {},
      \"roster\": [],
      \"slug\": \"ledger\",
      \"state\": \"draft\"
    }
  },
  \"decree_receipts\": {
    \"1\": \"c0225c3b8fabf543\"
  },
  \"decrees\": {
    \"1\": \"queued\"
  },
  \"parcels\": {
    \"1001\": {
      \"identity\": \"ada\",
      \"stasis\": false
    }
  },
  \"sessions\": {
    \"1001\": {
      \"id\": {
        \"incarnation\": 0,
        \"uid\": 1001
      },
      \"restart_count\": 0,
      \"status\": \"idle\"
    }
  },
  \"slots\": {
    \"1000\": {
      \"identity\": \"manor\",
      \"status\": \"active\"
    },
    \"1001\": {
      \"commission\": 1,
      \"identity\": \"ada\",
      \"status\": \"active\"
    },
    \"1002\": {
      \"identity\": \"grace\",
      \"status\": \"active\"
    }
  },
  \"webhooks\": [
    \"github\"
  ]
}
";
        let charter = sample_charter();
        assert_eq!(charter.render(), golden);
        assert_eq!(Charter::from_bytes(golden.as_bytes()).unwrap(), charter);
    }

    #[test]
    fn corrupt_json_is_hard_error_with_byte_offset() {
        // Truncated object: serde fails at EOF, reported past the last line.
        let bytes = b"{\n  \"charter_version\": 1,\n";
        let err = Charter::from_bytes(bytes).unwrap_err();
        match err {
            CharterError::Corrupt {
                line, byte_offset, ..
            } => {
                assert_eq!(line, 3);
                // No line 3 exists in the file — the offset clamps to EOF.
                assert_eq!(byte_offset, bytes.len());
            }
            other => panic!("expected Corrupt, got: {other}"),
        }
        // Byte offset lands mid-file for an interior error.
        let bytes = b"{\"charter_version\": 1, \"slots\": ???}";
        match Charter::from_bytes(bytes).unwrap_err() {
            CharterError::Corrupt {
                byte_offset,
                line,
                column,
                ..
            } => {
                assert_eq!((line, column), (1, 33));
                assert_eq!(byte_offset, 32);
                assert_eq!(&bytes[byte_offset..byte_offset + 1], b"?");
            }
            other => panic!("expected Corrupt, got: {other}"),
        }
    }

    #[test]
    fn unknown_field_is_corruption_not_tolerance() {
        let err = Charter::from_bytes(br#"{"charter_version": 1, "bogus": {}}"#).unwrap_err();
        assert!(matches!(err, CharterError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn future_version_is_rejected_before_migration() {
        let err = Charter::from_bytes(br#"{"charter_version": 5}"#).unwrap_err();
        assert!(
            matches!(
                err,
                CharterError::UnsupportedVersion {
                    found: 5,
                    supported: CHARTER_VERSION
                }
            ),
            "{err}"
        );
        let err =
            Charter::from_bytes(br#"{"charter_version": 5, "brand_new_group": 42}"#).unwrap_err();
        assert!(
            matches!(err, CharterError::UnsupportedVersion { found: 5, .. }),
            "{err}"
        );
        let err = Charter::from_bytes(br#"{"charter_version": 0}"#).unwrap_err();
        assert!(
            matches!(err, CharterError::UnsupportedVersion { found: 0, .. }),
            "{err}"
        );
    }

    #[test]
    fn invariant_violations_are_hard_errors() {
        // Parcel without slot record.
        let mut charter = Charter::empty();
        charter.parcels.insert(
            1001,
            ParcelLease {
                identity: name("ghost"),
                stasis: false,
            },
        );
        assert!(matches!(
            charter.validate(),
            Err(CharterError::Invariant { .. })
        ));
        // Agent ref to missing slot.
        let mut charter = Charter::empty();
        charter.agents.insert(name("ada"), AgentRef { slot: 1001 });
        assert!(charter.validate().is_err());
        // Agent ref bound to a slot held by another identity.
        let mut charter = Charter::empty();
        charter.allocate_slot(name("ada"), None).unwrap();
        charter
            .agents
            .insert(name("grace"), AgentRef { slot: 1001 });
        assert!(charter.validate().is_err());
        // Slot referencing unknown commission.
        let mut charter = Charter::empty();
        charter.allocate_slot(name("ada"), Some(9)).unwrap();
        assert!(charter.validate().is_err());
        // Slot uid outside the policy range.
        let mut charter = Charter::empty();
        charter
            .slots
            .insert(999, SlotRecord::new(name("squatter"), None));
        assert!(charter.validate().is_err());
        // Parcel identity mismatch.
        let mut charter = Charter::empty();
        charter.allocate_slot(name("ada"), None).unwrap();
        charter.parcels.insert(
            1001,
            ParcelLease {
                identity: name("grace"),
                stasis: false,
            },
        );
        assert!(charter.validate().is_err());
        // A consistent charter passes.
        assert!(sample_charter().validate().is_ok());
    }

    #[test]
    fn load_rejects_corrupt_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("charter.json");
        fs::write(&path, b"{\"charter_version\": ").unwrap();
        let err = Charter::load(&path).unwrap_err();
        assert!(matches!(err, CharterError::Corrupt { .. }), "{err}");
        // Missing file is I/O, not corruption.
        let err = Charter::load(&dir.path().join("nope.json")).unwrap_err();
        assert!(matches!(err, CharterError::Io { .. }), "{err}");
    }

    #[test]
    fn save_atomic_writes_canonical_file_and_no_temp_litter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("charter.json");
        let charter = sample_charter();
        charter.save_atomic(&path).unwrap();

        assert!(
            !Charter::temp_path(&path).exists(),
            "temp must be renamed away"
        );
        let on_disk = fs::read(&path).unwrap();
        assert_eq!(on_disk, charter.render().as_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644);
        }
    }

    /// Kill/restart resumability (CONVENTIONS §3 rule 6): build a charter,
    /// commit it atomically, then simulate `kill -9` — drop the in-memory
    /// state without any flush — reload from disk, and assert both equal
    /// state and byte-identical re-serialization.
    #[test]
    fn kill_restart_reload_is_byte_identical_and_state_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("charter.json");

        let mut charter = sample_charter();
        slots::hibernate(&mut charter.slots, 1002).unwrap();
        charter.parcels.insert(
            1002,
            ParcelLease {
                identity: name("grace"),
                stasis: true,
            },
        );
        charter.save_atomic(&path).unwrap();
        let committed_bytes = fs::read(&path).unwrap();

        drop(charter); // kill -9 equivalent: no flush, no Drop ceremony

        let recovered = Charter::load(&path).unwrap();
        let mut expected = sample_charter();
        slots::hibernate(&mut expected.slots, 1002).unwrap();
        expected.parcels.insert(
            1002,
            ParcelLease {
                identity: name("grace"),
                stasis: true,
            },
        );
        assert_eq!(recovered, expected, "state must survive kill/restart");
        assert_eq!(
            recovered.render().as_bytes(),
            committed_bytes,
            "re-serialization must be byte-identical"
        );
        // Hibernated slot kept its mapping through the restart (SPEC §10).
        assert_eq!(recovered.slots[&1002].identity, name("grace"));
        assert_eq!(recovered.slots[&1002].status, SlotStatus::Hibernated);
    }

    /// Torn write: a crash mid-`save_atomic` leaves a truncated temp file
    /// beside a perfectly good charter. The committed charter must survive
    /// untouched, load must ignore the temp, and the next save must succeed.
    #[test]
    fn torn_temp_write_leaves_previous_charter_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("charter.json");

        let v1 = sample_charter();
        v1.save_atomic(&path).unwrap();
        let v1_bytes = fs::read(&path).unwrap();

        // Simulate kill -9 halfway through writing v2's temp file.
        let mut v2 = v1.clone();
        v2.allocate_slot(name("alan"), None).unwrap();
        let v2_bytes = v2.render();
        let torn = &v2_bytes[..v2_bytes.len() / 2];
        fs::write(Charter::temp_path(&path), torn).unwrap();

        // The committed charter is exactly v1 — torn temp changes nothing.
        assert_eq!(fs::read(&path).unwrap(), v1_bytes);
        assert_eq!(Charter::load(&path).unwrap(), v1);

        // Next save completes cleanly and cleans up the stale temp.
        v2.save_atomic(&path).unwrap();
        assert_eq!(Charter::load(&path).unwrap(), v2);
        assert!(!Charter::temp_path(&path).exists());
    }

    #[test]
    fn slot_exhaustion_surfaces_through_charter() {
        let mut charter = Charter::empty();
        for i in 0..AGENT_SLOT_CAPACITY {
            charter
                .allocate_slot(name(&format!("agent-{i}")), None)
                .unwrap();
        }
        assert_eq!(
            charter.allocate_slot(name("overflow"), None),
            Err(SlotError::Exhausted {
                identity: name("overflow")
            })
        );
        // The failed allocation left no partial state behind.
        assert_eq!(charter.slots.len(), AGENT_SLOT_CAPACITY);
        assert!(charter.validate().is_ok());
    }

    #[test]
    fn active_commission_with_indenture_entries_roundtrips_json() {
        let mut charter = Charter::empty();
        let uid = charter.allocate_slot(name("scribe"), None).unwrap();
        let cid = charter.create_commission(name("watchtower")).unwrap();
        charter.assign_uid(cid, uid).unwrap();
        charter
            .apply_commission_event(
                cid,
                &CommissionEvent::IndenturePosted {
                    entries: BTreeSet::from([uid]),
                },
            )
            .unwrap();
        let rendered = charter.render();
        let reparsed = Charter::from_bytes(rendered.as_bytes()).unwrap();
        assert_eq!(reparsed, charter);
        assert_eq!(
            reparsed.commissions[&cid].state(),
            crate::commission::CommissionState::Active
        );
    }
}
