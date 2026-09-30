//! Charter state model + atomic canonical serde (SPEC §11.1; CONVENTIONS §3;
//! ADR-0004).
//!
//! In-memory state over `BTreeMap`/`BTreeSet` — iteration order is sorted,
//! always. Canonical human-readable JSON with sorted keys (serde_json over
//! `BTreeMap`), mandatory `charter_version: u32`, additive-only migrations
//! with explicit version steps. No wall-clock reads anywhere in state
//! derivation (timestamps are telemetry, never state inputs).
//!
//! **Normative content groups** (SPEC §11.1, v1 shape — later sprints extend
//! additively): [`slots`](crate::slots) (uid → identity record), `agents`
//! (registry refs), `commissions` (id → state placeholder), `parcels` (uid
//! lease records), `decrees` (queue status placeholder), `webhooks` (config
//! ref names).
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

use crate::sanitize::SafeName;
use crate::slots::{self, SlotError, SlotTable, FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};

/// Current charter schema version. Mandatory field (CONVENTIONS §3 rule 3);
/// migrations are additive-only with explicit version steps (ADR-0004).
pub const CHARTER_VERSION: u32 = 1;

/// Master state map — the typed form of `.hamlet/charter.json`.
///
/// All groups are `BTreeMap`/`BTreeSet` (sorted keys fall out naturally) and
/// serialize away when empty, so a fresh village renders exactly
/// `{"charter_version": 1}`. Parsing is strict (`deny_unknown_fields`):
/// unknown fields at a *supported* version are corruption, not tolerance —
/// newer files are rejected earlier by the version check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Commission states: ledger id → lifecycle placeholder. Sprint 06 owns
    /// the full state machine (SPEC §9); v1 records open/closed only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commissions: BTreeMap<u64, CommissionStatus>,

    /// Parcel lease records (`keep/census/parcel<uid>/`): uid → lease.
    /// `stasis` marks a patron whose home is parked under
    /// `tavern/patrons/` (SPEC §10); the slot mapping persists regardless.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parcels: BTreeMap<u32, ParcelLease>,

    /// Decree queue status: decree id → placeholder. Sprint 05 owns the
    /// decree vocabulary (SPEC §7); v1 records queue position only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub decrees: BTreeMap<u64, DecreeStatus>,

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

/// Commission lifecycle placeholder (sprint 06 replaces with the full
/// SPEC §9 state machine, additively).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommissionStatus {
    /// Commission is open (created through converging).
    Open,
    /// Commission closed (DEED/vault promotions complete, INDENTURE.md written).
    Closed,
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
            decrees: BTreeMap::new(),
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
        let charter = match serde_json::from_slice::<Self>(bytes) {
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
        Ok(())
    }
}

fn invariant(message: String) -> CharterError {
    CharterError::Invariant { message }
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
    let current = charter;
    // No steps exist yet: anything below CHARTER_VERSION that survived the
    // gate is a schema bug, not a migratable file.
    if current.charter_version != CHARTER_VERSION {
        return Err(invariant(format!(
            "no migration step from charter_version {} to {CHARTER_VERSION}",
            current.charter_version
        )));
    }
    current.validate()?;
    Ok(current)
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
        charter.commissions.insert(1, CommissionStatus::Open);
        charter.agents.insert(name("ada"), AgentRef { slot: ada });
        charter.parcels.insert(
            ada,
            ParcelLease {
                identity: name("ada"),
                stasis: false,
            },
        );
        charter.decrees.insert(1, DecreeStatus::Queued);
        charter.webhooks.insert(name("github"));
        charter
    }

    /// Committed golden vector: the exact bytes `hamlet init` writes for a
    /// fresh village (empty groups serialize away).
    #[test]
    fn empty_charter_render_golden() {
        assert_eq!(
            Charter::empty().render(),
            "{\n  \"charter_version\": 1\n}\n"
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
  \"charter_version\": 1,
  \"commissions\": {
    \"1\": \"open\"
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
        let err = Charter::from_bytes(br#"{"charter_version": 2}"#).unwrap_err();
        assert!(
            matches!(
                err,
                CharterError::UnsupportedVersion {
                    found: 2,
                    supported: CHARTER_VERSION
                }
            ),
            "{err}"
        );
        let err =
            Charter::from_bytes(br#"{"charter_version": 2, "brand_new_group": 42}"#).unwrap_err();
        assert!(
            matches!(err, CharterError::UnsupportedVersion { found: 2, .. }),
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
}
