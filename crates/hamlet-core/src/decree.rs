//! Decree vocabulary + file encodings (SPEC §7; encodings \[DEV\] → ADR-0026).
//!
//! The Mayor is a stateless executive: it emits **decrees as files** and the
//! daemon interprets them. This module owns the pure half of that contract —
//! the normative vocabulary ([`Decree`]), the on-disk encodings (one JSON
//! file per decree in `keep/census/manor/home/decrees/`, ADR-0026 cl. 1–2),
//! the identity/ordering key ([`DecreeId`]), the daemon acknowledgment file
//! ([`DecreeAck`]), and the typed rejection vocabulary ([`RejectReason`]).
//! Writing the atomic temp+rename, watching the directory, and *executing* a
//! decree belong to `hamlet-daemon::mayor` (singular governor,
//! CONVENTIONS §2).
//!
//! # File contract (ADR-0026 cl. 2)
//!
//! ```text
//! keep/census/manor/home/decrees/
//! ├── d-000000000007-a1b2c3d4e5f60718.json   ← Mayor-written decree (atomic temp+rename)
//! ├── d-000000000007-a1b2c3d4e5f60718.ack    ← daemon-written acknowledgment, next to it
//! └── .rejected/                              ← unparseable files, quarantined by rename
//! ```
//!
//! - **Why `manor/home/`**: SPEC §4 mounts exactly one tree rw into the Mayor
//!   container — `keep/census/manor/home/` → `/home/1000/`. The Mayor may
//!   write *nowhere else*, so the decree directory is `decrees/` inside its
//!   home (`/home/1000/decrees/`), which is also where ADR-0019 cl. 5 already
//!   routes `Reaction::DecreeWrite`.
//! - `<decree_id>` renders as `d-{seq:012}-{digest16}`, so the file name is
//!   simultaneously a valid [`SafeName`] (single `.` extension, charset
//!   `[A-Za-z0-9._-]`) and lexicographically **monotonic in `seq`**.
//! - **Ordering is the charter counter, never the clock** (CONVENTIONS §3
//!   r4): `seq` must equal the charter's next decree sequence at admission
//!   time ([`crate::charter::Charter::admit_decree`]). File mtimes are never
//!   read, and a zero-padded name makes lexicographic order agree with `seq`.
//!
//! # Identity, replay & idempotence (ADR-0026 cl. 3–4)
//!
//! `decree_id` binds two things:
//!
//! - `seq` — the monotonic charter counter (ordering + replay key).
//! - `digest` — 16 lowercase hex chars over the **canonical body with
//!   `decree_id` removed** ([`Decree::digest`]). Re-emitting an identical
//!   file is an idempotent no-op (same `seq` + same digest); re-using a
//!   `seq` for different content is refused (`replay_mismatch`) rather than
//!   silently dropped. The digest is FNV-1a/64 — a *collision detector* for
//!   Mayor-authored files, not a security primitive (see ADR-0026 cl. 3).
//!
//! Everything here is pure (ADR-0001): no I/O, no clock, no process access.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mail::is_slot_uid;
use crate::sanitize::SafeName;

/// Schema version stamped into every decree file (ADR-0026 cl. 2).
pub const DECREE_VERSION: u32 = 1;
/// Schema version stamped into every acknowledgment file (ADR-0026 cl. 5).
pub const ACK_VERSION: u32 = 1;
/// Maximum accepted decree file size, bytes (ADR-0026 cl. 2).
pub const MAX_DECREE_BYTES: usize = 16 * 1024;
/// Maximum acknowledgment file size, bytes (ADR-0026 cl. 5).
pub const MAX_ACK_BYTES: usize = 4 * 1024;
/// Hex characters in the content digest (ADR-0026 cl. 3).
pub const DIGEST_HEX_LEN: usize = 16;
/// File extension of a decree file.
pub const DECREE_EXT: &str = "json";
/// File extension of an acknowledgment file.
pub const ACK_EXT: &str = "ack";
/// Maximum `summary`/`reason` length in chars (ADR-0026 cl. 1).
pub const MAX_SUMMARY_LEN: usize = 280;
/// Maximum `reason` length in chars (ADR-0026 cl. 1).
pub const MAX_REASON_LEN: usize = 280;
/// Maximum `directive` length in bytes (ADR-0026 cl. 1).
pub const MAX_DIRECTIVE_BYTES: usize = 8 * 1024;
/// Schema version stamped into every escalation notice (ADR-0029 cl. 4).
pub const ESCALATION_VERSION: u32 = 1;
/// Maximum escalation notice size, bytes (ADR-0029 cl. 4).
pub const MAX_ESCALATION_BYTES: usize = 8 * 1024;
/// Maximum `evidence_refs` entries in one escalation (ADR-0026 cl. 1).
pub const MAX_EVIDENCE_REFS: usize = 8;
/// Maximum components in one village-relative path (ADR-0026 cl. 1).
pub const MAX_PATH_COMPONENTS: usize = 8;
/// Maximum length of one village-relative path component, chars
/// (CONVENTIONS §4 cap, reused so evidence labels and file names agree).
pub const MAX_PATH_COMPONENT_LEN: usize = 64;
/// Every promotion decree's `freight_path` must live under this prefix
/// (ADR-0026 cl. 1 — SPEC §7 validation loop reads `/var/freight/` only).
pub const FREIGHT_PREFIX: &str = "courier/freight";

/// Why a decree, decree file, or acknowledgment was rejected. The first
/// failure in check order wins; the variant order is part of the committed
/// golden contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecreeError {
    /// Decree file exceeds [`MAX_DECREE_BYTES`] / ack exceeds [`MAX_ACK_BYTES`].
    Oversize {
        /// Observed size, bytes.
        bytes: usize,
    },
    /// JSON does not match the decree schema (message from serde).
    Schema(String),
    /// `decree_version` / `ack_version` is not the supported version.
    UnsupportedVersion(u32),
    /// `decree_id` is not `d-<seq:012>-<16 hex>` or `seq` is 0.
    InvalidId(String),
    /// The stamped digest does not match the recomputed content digest.
    DigestMismatch {
        /// Digest carried by the file.
        stamped: String,
        /// Digest recomputed from the body.
        computed: String,
    },
    /// A bounded text field is empty, overlong, or carries a control char.
    InvalidText {
        /// Field name (stable identity).
        field: &'static str,
        /// Which rule failed.
        reason: &'static str,
    },
    /// A UID is outside {1000, 1001–2000} (SPEC §4).
    InvalidUid(u32),
    /// A commission index is 0 (ledger commissions are `commission<n>`, n ≥ 1).
    InvalidCommission,
    /// A village-relative path is malformed (empty component, traversal,
    /// bad charset, too deep, overlong).
    InvalidPath {
        /// The offending path, as written.
        path: String,
        /// Which rule failed.
        reason: &'static str,
    },
    /// A village-relative path is well-formed but outside the region its
    /// field requires (e.g. a `freight_path` not under [`FREIGHT_PREFIX`]).
    PathOutOfRegion {
        /// The offending path, as written.
        path: String,
    },
    /// `evidence_refs` was empty.
    EmptyEvidence,
    /// `evidence_refs` exceeded [`MAX_EVIDENCE_REFS`].
    TooManyEvidence {
        /// Number of refs supplied.
        count: usize,
    },
}

impl fmt::Display for DecreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { bytes } => write!(f, "decree artifact oversize ({bytes} bytes)"),
            Self::Schema(msg) => write!(f, "decree schema violation: {msg}"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported decree/ack version {v} (this build writes and accepts \
                 {DECREE_VERSION})"
            ),
            Self::InvalidId(raw) => write!(f, "invalid decree_id {raw:?}"),
            Self::DigestMismatch { stamped, computed } => write!(
                f,
                "decree digest mismatch: stamped {stamped}, computed {computed}"
            ),
            Self::InvalidText { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
            Self::InvalidUid(uid) => write!(f, "uid {uid} is not a charter slot"),
            Self::InvalidCommission => write!(f, "commission index must be >= 1"),
            Self::InvalidPath { path, reason } => {
                write!(f, "invalid village path {path:?}: {reason}")
            }
            Self::PathOutOfRegion { path } => {
                write!(f, "village path {path:?} is outside the required region")
            }
            Self::EmptyEvidence => write!(f, "escalation evidence_refs must not be empty"),
            Self::TooManyEvidence { count } => {
                write!(
                    f,
                    "escalation evidence_refs holds {count} refs (max {MAX_EVIDENCE_REFS})"
                )
            }
        }
    }
}

impl std::error::Error for DecreeError {}

// ---------------------------------------------------------------------------
// Content digest (ADR-0026 cl. 3)
// ---------------------------------------------------------------------------

/// FNV-1a/64 over `bytes`, rendered as 16 lowercase hex chars.
///
/// Deliberately dependency-free: SPEC §3 locks the crate stack and a new
/// crypto crate would need its own ADR. This digest detects *accidental or
/// naive* content drift between a decree file and its recorded id — the
/// trust model here is the ADR-0019 model (the Mayor authors these files
/// deliberately), not adversarial tampering. If adversarial digest
/// resistance is ever required, that is a superseding ADR adding `sha2`.
#[must_use]
pub fn content_digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Village-relative path (ADR-0026 cl. 1)
// ---------------------------------------------------------------------------

/// A validated village-relative path label (`a/b/c`), used by decree targets
/// and by the context packet's evidence references.
///
/// Every component is 1–[`MAX_PATH_COMPONENT_LEN`] chars over
/// `[A-Za-z0-9._-]`, is not `.`/`..`, and does not start with `-`; there are
/// at most [`MAX_PATH_COMPONENTS`] of them. `.`/`..` are rejected *and*
/// separators can only appear as `/`, so [`TreeRelPath::join_under`] can
/// never leave the root — traversal is blocked at the type boundary
/// (SPEC §2 name-safety, ADR-0002/0003).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeRelPath(Vec<String>);

impl TreeRelPath {
    /// Parse and validate a village-relative path.
    ///
    /// # Errors
    ///
    /// [`DecreeError::InvalidPath`] naming the first offending component.
    pub fn parse(raw: &str) -> Result<Self, DecreeError> {
        let invalid = |reason: &'static str| DecreeError::InvalidPath {
            path: raw.to_owned(),
            reason,
        };
        if raw.is_empty() {
            return Err(invalid("empty path"));
        }
        if raw.starts_with('/') {
            return Err(invalid("absolute path"));
        }
        if raw.ends_with('/') {
            return Err(invalid("trailing separator"));
        }
        if raw.contains('\\') || raw.contains('\0') {
            return Err(invalid("backslash or NUL"));
        }
        let components: Vec<String> = raw.split('/').map(str::to_owned).collect();
        if components.len() > MAX_PATH_COMPONENTS {
            return Err(invalid("too many components"));
        }
        for component in &components {
            let reason: &'static str = if component.is_empty() {
                "empty component"
            } else if component == "." || component == ".." {
                "traversal component"
            } else if component.starts_with('-') {
                "component starts with '-'"
            } else if component.len() > MAX_PATH_COMPONENT_LEN {
                "component too long"
            } else if !component
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            {
                "component outside [A-Za-z0-9._-]"
            } else {
                continue;
            };
            return Err(invalid(reason));
        }
        Ok(Self(components))
    }

    /// The validated components, in order.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.0
    }

    /// True when the path is `prefix` itself or lies under it
    /// (`courier/freight/…`).
    #[must_use]
    pub fn starts_with(&self, prefix: &str) -> bool {
        let prefix_components: Vec<&str> = prefix.split('/').collect();
        self.0.len() >= prefix_components.len()
            && prefix_components
                .iter()
                .zip(self.0.iter())
                .all(|(want, have)| *want == have.as_str())
    }

    /// Join under `root`, re-checking containment. Returns `None` if the
    /// result would not stay inside `root` — belt and braces over the type
    /// invariant, because the daemon resolves these against a live tree
    /// (SPEC §2 name-safety).
    #[must_use]
    pub fn join_under(&self, root: &Path) -> Option<PathBuf> {
        let mut path = root.to_path_buf();
        for component in &self.0 {
            path.push(component);
        }
        path.starts_with(root).then_some(path)
    }
}

impl fmt::Display for TreeRelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

impl Serialize for TreeRelPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.join("/"))
    }
}

impl<'de> Deserialize<'de> for TreeRelPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Decree identity (ADR-0026 cl. 3)
// ---------------------------------------------------------------------------

/// The identity of one decree: a monotonic charter sequence plus the content
/// digest it was admitted with. Rendered `d-{seq:012}-{digest16}`; the same
/// token names the decree file and its acknowledgment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DecreeId {
    seq: u64,
    digest: String,
}

impl DecreeId {
    /// Build + validate: `seq` ≥ 1, `digest` exactly [`DIGEST_HEX_LEN`]
    /// lowercase hex chars.
    ///
    /// # Errors
    ///
    /// [`DecreeError::InvalidId`].
    pub fn new(seq: u64, digest: impl Into<String>) -> Result<Self, DecreeError> {
        let digest = digest.into();
        let valid_digest = digest.len() == DIGEST_HEX_LEN
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if seq == 0 || !valid_digest {
            return Err(DecreeError::InvalidId(format!("d-{seq:012}-{digest}")));
        }
        Ok(Self { seq, digest })
    }

    /// Derive the id of a decree body admitted at `seq`: the digest is the
    /// content digest of the canonical body (see [`Decree::digest`]).
    ///
    /// # Errors
    ///
    /// [`DecreeError::InvalidId`] when `seq` is 0.
    pub fn derive(seq: u64, digest: &str) -> Result<Self, DecreeError> {
        Self::new(seq, digest)
    }

    /// Parse the canonical token form `d-<12-digit seq>-<16 hex>`.
    ///
    /// # Errors
    ///
    /// [`DecreeError::InvalidId`].
    pub fn parse(raw: &str) -> Result<Self, DecreeError> {
        let invalid = || DecreeError::InvalidId(raw.to_owned());
        let rest = raw.strip_prefix("d-").ok_or_else(invalid)?;
        let (seq_text, digest) = rest.split_once('-').ok_or_else(invalid)?;
        if seq_text.len() != 12 || !seq_text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let seq: u64 = seq_text.parse().map_err(|_| invalid())?;
        Self::new(seq, digest).map_err(|_| invalid())
    }

    /// The monotonic charter sequence (1-based; never derived from a clock).
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The content digest (16 lowercase hex chars).
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Canonical token form — the file stem of the decree and its ack.
    #[must_use]
    pub fn render(&self) -> String {
        format!("d-{:012}-{}", self.seq, self.digest)
    }

    /// The decree file name `<decree_id>.json` (a valid [`SafeName`]).
    ///
    /// # Errors
    ///
    /// [`DecreeError::InvalidId`] if the token cannot be sanitized (cannot
    /// happen for a valid [`DecreeId`]; kept so no `unwrap` is reachable).
    pub fn decree_file_name(&self) -> Result<SafeName, DecreeError> {
        file_name(&self.render(), DECREE_EXT)
    }

    /// The acknowledgment file name `<decree_id>.ack`.
    ///
    /// # Errors
    ///
    /// As [`DecreeId::decree_file_name`].
    pub fn ack_file_name(&self) -> Result<SafeName, DecreeError> {
        file_name(&self.render(), ACK_EXT)
    }
}

impl fmt::Display for DecreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl Serialize for DecreeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.render())
    }
}

impl<'de> Deserialize<'de> for DecreeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Build `<stem>.<ext>` as a [`SafeName`] (CONVENTIONS §4).
fn file_name(stem: &str, ext: &str) -> Result<SafeName, DecreeError> {
    crate::sanitize::sanitize_name(&format!("{stem}.{ext}"))
        .map_err(|_| DecreeError::InvalidId(format!("{stem}.{ext}")))
}

/// True when `name` is exactly a well-formed decree file name. Used by the
/// watcher classification (ADR-0019 cl. 5, amended by ADR-0026 cl. 6) so a
/// daemon-written `.ack` next to a decree never re-enters as a new decree.
#[must_use]
pub fn is_decree_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(&format!(".{DECREE_EXT}")) else {
        return false;
    };
    // A hidden name is never a decree (writer temps stage dot-prefixed).
    if stem.starts_with('.') {
        return false;
    }
    DecreeId::parse(stem).is_ok()
}

/// Parse a decree file name back to its id.
///
/// # Errors
///
/// [`DecreeError::InvalidId`].
pub fn decree_id_from_file_name(name: &str) -> Result<DecreeId, DecreeError> {
    let stem = name
        .strip_suffix(&format!(".{DECREE_EXT}"))
        .ok_or_else(|| DecreeError::InvalidId(name.to_owned()))?;
    DecreeId::parse(stem)
}

// ---------------------------------------------------------------------------
// The decree vocabulary (SPEC §7; ADR-0026 cl. 1)
// ---------------------------------------------------------------------------

/// One normative Mayor decree. The `decree_type` tag is the wire identity of
/// the variant; fields within a variant are declared alphabetically and every
/// rendering goes through a sorted-key [`serde_json::Value`] so the canonical
/// bytes are declaration-order independent (CONVENTIONS §3 r3).
///
/// The `decompose commission → INDENTURE.md` decree of SPEC §7 is
/// deliberately **absent**: it is Sprint 06 scope (OBJECTIVE scope boundary)
/// and will be added as an eighth variant by an ADR amendment — the tag +
/// field shape make that purely additive (ADR-0026 cl. 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decree_type", rename_all = "snake_case")]
pub enum Decree {
    /// Approve a deliverable for promotion into `keep/vault/`
    /// (SPEC §7 validation loop).
    ApprovePromotion {
        /// Identity + sequence of this decree (ADR-0026 cl. 3).
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Vault artifact name — a sanitized basename (SPEC §11.3).
        artifact_name: SafeName,
        /// Freight-relative source of the artifact, under [`FREIGHT_PREFIX`].
        freight_path: TreeRelPath,
    },
    /// Hibernate one agent: stop the session and park its home in stasis
    /// (SPEC §10; Sprint 07 owns the copy).
    HibernateAgent {
        /// Identity + sequence of this decree.
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Target slot UID.
        uid: u32,
    },
    /// Escalate to a human operator (SPEC §7) — the daemon writes the notice
    /// mail; nothing else happens without a person.
    EscalateToHuman {
        /// Identity + sequence of this decree.
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Evidence labels justifying the escalation (1..=[`MAX_EVIDENCE_REFS`]
        /// village-relative paths — the daemon re-resolves each one).
        evidence_refs: Vec<TreeRelPath>,
        /// Single-line human summary, 1–[`MAX_SUMMARY_LEN`] chars.
        summary: String,
    },
    /// Provision one agent parcel from its register brief + blueprint
    /// (SPEC §5, §7).
    ProvisionAgent {
        /// Immutable blueprint declaration in `guild/blueprints/`.
        blueprint_ref: SafeName,
        /// Ledger commission the agent serves, or `null` for an idle agent.
        #[serde(default)]
        commission: Option<u64>,
        /// Identity + sequence of this decree.
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Immutable agent brief stem in `guild/register/`.
        register_ref: SafeName,
    },
    /// Reject a deliverable: no promotion, reason returned to the agents.
    RejectPromotion {
        /// Identity + sequence of this decree.
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Single-line rejection reason, 1–[`MAX_REASON_LEN`] chars.
        reason: String,
        /// Freight-relative source under evaluation, under [`FREIGHT_PREFIX`].
        freight_path: TreeRelPath,
    },
    /// Reassign a bounded task to a running slot (SPEC §7, §9.2).
    ReassignTask {
        /// Identity + sequence of this decree.
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Commission whose `INDENTURE.md` holds the task.
        commission: u64,
        /// Task id inside that INDENTURE.md (Sprint 06 owns the file format;
        /// this is the reference, not the prose).
        entry: SafeName,
        /// Target slot UID.
        uid: u32,
    },
    /// Steer a live session out of band (SPEC §6 Divine Interruption).
    SteerSession {
        /// Identity + sequence of this decree.
        decree_id: DecreeId,
        /// Must equal [`DECREE_VERSION`].
        decree_version: u32,
        /// Directive text, 1–[`MAX_DIRECTIVE_BYTES`] bytes, LF-only.
        directive: String,
        /// Target slot UID.
        uid: u32,
    },
}

/// Stable wire identity of a decree variant (the `decree_type` value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DecreeKind {
    /// [`Decree::ApprovePromotion`]
    ApprovePromotion,
    /// [`Decree::HibernateAgent`]
    HibernateAgent,
    /// [`Decree::EscalateToHuman`]
    EscalateToHuman,
    /// [`Decree::ProvisionAgent`]
    ProvisionAgent,
    /// [`Decree::RejectPromotion`]
    RejectPromotion,
    /// [`Decree::ReassignTask`]
    ReassignTask,
    /// [`Decree::SteerSession`]
    SteerSession,
}

impl DecreeKind {
    /// Every kind in declaration order — the closed vocabulary (ADR-0026 cl. 1).
    pub const ALL: &'static [DecreeKind] = &[
        DecreeKind::ApprovePromotion,
        DecreeKind::HibernateAgent,
        DecreeKind::EscalateToHuman,
        DecreeKind::ProvisionAgent,
        DecreeKind::RejectPromotion,
        DecreeKind::ReassignTask,
        DecreeKind::SteerSession,
    ];

    /// Parse a `decree_type` token.
    ///
    /// # Errors
    ///
    /// [`DecreeError::Schema`] for any token outside the closed set.
    pub fn from_token(token: &str) -> Result<Self, DecreeError> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.token() == token)
            .ok_or_else(|| DecreeError::Schema(format!("unknown decree_type {token:?}")))
    }

    /// Every accepted JSON key of this variant (sorted). The parse boundary
    /// refuses any key outside this set — an explicit allowlist instead of
    /// serde's `deny_unknown_fields`, which is not available on the
    /// variants of an internally tagged enum (ADR-0026 cl. 2).
    #[must_use]
    pub fn allowed_fields(self) -> &'static [&'static str] {
        match self {
            Self::ApprovePromotion => &[
                "artifact_name",
                "decree_id",
                "decree_type",
                "decree_version",
                "freight_path",
            ],
            Self::HibernateAgent => &["decree_id", "decree_type", "decree_version", "uid"],
            Self::EscalateToHuman => &[
                "decree_id",
                "decree_type",
                "decree_version",
                "evidence_refs",
                "summary",
            ],
            Self::ProvisionAgent => &[
                "blueprint_ref",
                "commission",
                "decree_id",
                "decree_type",
                "decree_version",
                "register_ref",
            ],
            Self::RejectPromotion => &[
                "decree_id",
                "decree_type",
                "decree_version",
                "freight_path",
                "reason",
            ],
            Self::ReassignTask => &[
                "commission",
                "decree_id",
                "decree_type",
                "decree_version",
                "entry",
                "uid",
            ],
            Self::SteerSession => &[
                "decree_id",
                "decree_type",
                "decree_version",
                "directive",
                "uid",
            ],
        }
    }

    /// The stable `decree_type` token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::ApprovePromotion => "approve_promotion",
            Self::HibernateAgent => "hibernate_agent",
            Self::EscalateToHuman => "escalate_to_human",
            Self::ProvisionAgent => "provision_agent",
            Self::RejectPromotion => "reject_promotion",
            Self::ReassignTask => "reassign_task",
            Self::SteerSession => "steer_session",
        }
    }
}

impl fmt::Display for DecreeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

impl Decree {
    /// Stable kind of this decree.
    #[must_use]
    pub fn kind(&self) -> DecreeKind {
        match self {
            Self::ApprovePromotion { .. } => DecreeKind::ApprovePromotion,
            Self::HibernateAgent { .. } => DecreeKind::HibernateAgent,
            Self::EscalateToHuman { .. } => DecreeKind::EscalateToHuman,
            Self::ProvisionAgent { .. } => DecreeKind::ProvisionAgent,
            Self::RejectPromotion { .. } => DecreeKind::RejectPromotion,
            Self::ReassignTask { .. } => DecreeKind::ReassignTask,
            Self::SteerSession { .. } => DecreeKind::SteerSession,
        }
    }

    /// Identity of this decree.
    #[must_use]
    pub fn id(&self) -> &DecreeId {
        match self {
            Self::ApprovePromotion { decree_id, .. }
            | Self::HibernateAgent { decree_id, .. }
            | Self::EscalateToHuman { decree_id, .. }
            | Self::ProvisionAgent { decree_id, .. }
            | Self::RejectPromotion { decree_id, .. }
            | Self::ReassignTask { decree_id, .. }
            | Self::SteerSession { decree_id, .. } => decree_id,
        }
    }

    /// Stamp `seq` onto this body, deriving the content digest — the exact
    /// transformation a stateless Mayor applies before writing the file
    /// (ADR-0026 cl. 3).
    ///
    /// # Errors
    ///
    /// [`DecreeError::InvalidId`] when `seq` is 0.
    pub fn stamp(self, seq: u64) -> Result<Self, DecreeError> {
        let id = DecreeId::derive(seq, &self.digest())?;
        Ok(self.with_id(id))
    }

    /// Replace this body's identity (used by [`Decree::stamp`] and by the
    /// parse-time re-stamp check).
    #[must_use]
    pub fn with_id(self, id: DecreeId) -> Self {
        match self {
            Self::ApprovePromotion {
                artifact_name,
                freight_path,
                ..
            } => Self::ApprovePromotion {
                decree_id: id,
                decree_version: DECREE_VERSION,
                artifact_name,
                freight_path,
            },
            Self::HibernateAgent { uid, .. } => Self::HibernateAgent {
                decree_id: id,
                decree_version: DECREE_VERSION,
                uid,
            },
            Self::EscalateToHuman {
                evidence_refs,
                summary,
                ..
            } => Self::EscalateToHuman {
                decree_id: id,
                decree_version: DECREE_VERSION,
                evidence_refs,
                summary,
            },
            Self::ProvisionAgent {
                blueprint_ref,
                commission,
                register_ref,
                ..
            } => Self::ProvisionAgent {
                blueprint_ref,
                commission,
                decree_id: id,
                decree_version: DECREE_VERSION,
                register_ref,
            },
            Self::RejectPromotion {
                freight_path,
                reason,
                ..
            } => Self::RejectPromotion {
                decree_id: id,
                decree_version: DECREE_VERSION,
                freight_path,
                reason,
            },
            Self::ReassignTask {
                commission,
                entry,
                uid,
                ..
            } => Self::ReassignTask {
                commission,
                decree_id: id,
                decree_version: DECREE_VERSION,
                entry,
                uid,
            },
            Self::SteerSession { directive, uid, .. } => Self::SteerSession {
                decree_id: id,
                decree_version: DECREE_VERSION,
                directive,
                uid,
            },
        }
    }

    /// The canonical **digest input**: this document serialized as compact
    /// JSON with sorted keys and `decree_id` removed (ADR-0026 cl. 3). The
    /// content digest is FNV-1a/64 over exactly these bytes.
    #[must_use]
    pub fn digest(&self) -> String {
        content_digest(self.digest_input().as_bytes())
    }

    /// Canonical compact sorted-key JSON of the body without `decree_id`
    /// (exposed so tests and the digest can be pinned to the same bytes).
    #[must_use]
    pub fn digest_input(&self) -> String {
        let mut value =
            serde_json::to_value(self).expect("derive-based Decree serialization is infallible");
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("decree_id");
        }
        serde_json::to_string(&value).expect("Value serialization is infallible")
    }

    /// Validate a parsed or constructed decree: version, target ranges,
    /// bounded text, path regions, and the content-digest binding.
    ///
    /// # Errors
    ///
    /// The first [`DecreeError`] in check order (version → id → per-variant
    /// fields → digest binding).
    pub fn validate(&self) -> Result<(), DecreeError> {
        match self {
            Self::ApprovePromotion {
                decree_version,
                freight_path,
                ..
            } => {
                check_version(*decree_version)?;
                check_freight_path(freight_path)?;
            }
            Self::HibernateAgent {
                decree_version,
                uid,
                ..
            } => {
                check_version(*decree_version)?;
                check_uid(*uid)?;
            }
            Self::EscalateToHuman {
                decree_version,
                evidence_refs,
                summary,
                ..
            } => {
                check_version(*decree_version)?;
                check_single_line(summary, MAX_SUMMARY_LEN, "summary")?;
                if evidence_refs.is_empty() {
                    return Err(DecreeError::EmptyEvidence);
                }
                if evidence_refs.len() > MAX_EVIDENCE_REFS {
                    return Err(DecreeError::TooManyEvidence {
                        count: evidence_refs.len(),
                    });
                }
            }
            Self::ProvisionAgent {
                blueprint_ref: _,
                commission,
                decree_version,
                ..
            } => {
                check_version(*decree_version)?;
                check_commission(*commission)?;
            }
            Self::RejectPromotion {
                decree_version,
                freight_path,
                reason,
                ..
            } => {
                check_version(*decree_version)?;
                check_single_line(reason, MAX_REASON_LEN, "reason")?;
                check_freight_path(freight_path)?;
            }
            Self::ReassignTask {
                commission,
                decree_version,
                uid,
                ..
            } => {
                check_version(*decree_version)?;
                check_commission(Some(*commission))?;
                check_uid(*uid)?;
            }
            Self::SteerSession {
                decree_version,
                directive,
                uid,
                ..
            } => {
                check_version(*decree_version)?;
                check_multi_line(directive, MAX_DIRECTIVE_BYTES, "directive")?;
                check_uid(*uid)?;
            }
        }
        // Identity/content binding last: the recomputed digest must equal the
        // stamped one (ADR-0026 cl. 3).
        let computed = self.digest();
        let stamped = self.id().digest();
        if computed != stamped {
            return Err(DecreeError::DigestMismatch {
                stamped: stamped.to_owned(),
                computed,
            });
        }
        Ok(())
    }

    /// Parse a decree file's contents: size cap → schema → validate.
    ///
    /// # Errors
    ///
    /// [`DecreeError::Oversize`], [`DecreeError::Schema`], or whatever
    /// [`Decree::validate`] reports.
    pub fn parse(contents: &str) -> Result<Self, DecreeError> {
        if contents.len() > MAX_DECREE_BYTES {
            return Err(DecreeError::Oversize {
                bytes: contents.len(),
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(contents).map_err(|e| DecreeError::Schema(e.to_string()))?;
        let decree = Self::from_strict_value(value)?;
        decree.validate()?;
        Ok(decree)
    }

    /// Deserialize from a [`serde_json::Value`] through the strict key
    /// allowlist ([`DecreeKind::allowed_fields`]) — unknown keys are a
    /// schema violation, never silently dropped.
    ///
    /// # Errors
    ///
    /// [`DecreeError::Schema`] for a non-object document, an unknown
    /// `decree_type`, a foreign key, or a serde shape violation.
    pub fn from_strict_value(value: serde_json::Value) -> Result<Self, DecreeError> {
        let serde_json::Value::Object(map) = &value else {
            return Err(DecreeError::Schema(
                "decree document must be a JSON object".to_owned(),
            ));
        };
        let tag = map
            .get("decree_type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DecreeError::Schema("decree_type must be a string".to_owned()))?;
        let kind = DecreeKind::from_token(tag)?;
        let allowed = kind.allowed_fields();
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(DecreeError::Schema(format!(
                    "unknown field {key:?} for decree_type {tag:?}"
                )));
            }
        }
        serde_json::from_value(value).map_err(|e| DecreeError::Schema(e.to_string()))
    }

    /// Canonical rendering: pretty JSON, 2-space indent, **sorted keys at
    /// every level** (a sorted-key [`serde_json::Value`] round-trip),
    /// trailing newline — the bytes the Mayor renames into place.
    ///
    /// # Errors
    ///
    /// [`DecreeError::Schema`] (cannot fail for validated values).
    pub fn render(&self) -> Result<String, DecreeError> {
        let value = serde_json::to_value(self).map_err(|e| DecreeError::Schema(e.to_string()))?;
        let mut out =
            serde_json::to_string_pretty(&value).map_err(|e| DecreeError::Schema(e.to_string()))?;
        out.push('\n');
        Ok(out)
    }
}

fn check_version(version: u32) -> Result<(), DecreeError> {
    if version != DECREE_VERSION {
        return Err(DecreeError::UnsupportedVersion(version));
    }
    Ok(())
}

fn check_uid(uid: u32) -> Result<(), DecreeError> {
    if is_slot_uid(uid) {
        Ok(())
    } else {
        Err(DecreeError::InvalidUid(uid))
    }
}

fn check_commission(commission: Option<u64>) -> Result<(), DecreeError> {
    match commission {
        Some(n) if n < 1 => Err(DecreeError::InvalidCommission),
        _ => Ok(()),
    }
}

/// A `freight_path` must sit **under** [`FREIGHT_PREFIX`] (SPEC §7 reads
/// deliverables from `/var/freight/` only) and address a file inside a
/// sandbox (≥ 3 components).
fn check_freight_path(path: &TreeRelPath) -> Result<(), DecreeError> {
    if !path.starts_with(FREIGHT_PREFIX) || path.components().len() < 3 {
        return Err(DecreeError::PathOutOfRegion {
            path: path.to_string(),
        });
    }
    Ok(())
}

/// Single-line bounded text: non-empty, ≤ `max` chars, no control chars
/// (rejects CR/LF/NUL/tab — the ADR-0019 cl. 1 injection rule).
fn check_single_line(value: &str, max: usize, field: &'static str) -> Result<(), DecreeError> {
    let reason = if value.is_empty() {
        "must not be empty"
    } else if value.chars().count() > max {
        "too long"
    } else if value.chars().any(char::is_control) {
        "must not contain control characters"
    } else {
        return Ok(());
    };
    Err(DecreeError::InvalidText { field, reason })
}

/// Multi-line bounded text: non-empty, ≤ `max` bytes, at least one
/// non-whitespace char, LF-only (**CR is a rejection**, ADR-0019 cl. 1).
fn check_multi_line(value: &str, max: usize, field: &'static str) -> Result<(), DecreeError> {
    let reason = if value.is_empty() {
        "must not be empty"
    } else if value.len() > max {
        "too long"
    } else if value.contains('\r') {
        "must not contain CR (LF-only contract)"
    } else if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        "must not contain control characters"
    } else if value.trim().is_empty() {
        "must not be blank"
    } else {
        return Ok(());
    };
    Err(DecreeError::InvalidText { field, reason })
}

// ---------------------------------------------------------------------------
// Acknowledgment (ADR-0026 cl. 5)
// ---------------------------------------------------------------------------

/// Typed rejection vocabulary written by the daemon into `<decree_id>.ack`
/// (ADR-0026 cl. 5). Tokens are stable identities (never renumber, only
/// add) — the Mayor reads them on the next wake and the daemon traces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The `seq` was already admitted with a **different** content digest —
    /// the Mayor reused a sequence number (ADR-0026 cl. 3).
    ReplayMismatch,
    /// The file parsed but its `decree_version` is not supported.
    UnsupportedVersion,
    /// The file exceeded [`MAX_DECREE_BYTES`].
    Oversize,
    /// The file is not a decree document at all (schema violation).
    Malformed,
    /// A structural rule of the target is violated (e.g. a `freight_path`
    /// outside `courier/freight/`, an empty evidence list).
    InvalidTarget,
    /// The target UID holds no charter slot (SPEC §4).
    UnknownSlot,
    /// The decree was valid but execution failed (slot exhaustion, runtime
    /// failure); the charter records it as failed.
    ExecutionFailed,
    /// The daemon parsed and admitted the decree, but the subsystem that
    /// owns it has not landed yet (Sprint 06 promotion). The charter status
    /// stays `queued`, so the decree is retried when the seam arrives —
    /// refusing loudly beats acknowledging work that did not happen.
    Deferred,
    /// The decree's sequence is not the charter's next sequence: the Mayor
    /// used an out-of-order or stale counter value. Ordering is the charter
    /// counter, never a clock (CONVENTIONS §3 r4), so the file can never be
    /// executed — and its sequence is *not* consumed, leaving the Mayor free
    /// to re-issue the same decision under the correct one.
    OutOfOrder,
    /// The file is not attributable to the Mayor slot (ADR-0029 cl. 1:
    /// UID 1000 is the only intended author). Raised when the decree file's
    /// owner is not the owner of `manor/home/decrees/`, or when the decree
    /// directory itself is not a plain directory under the manor home — a
    /// symlink planted in its place, a foreign writer, a group/world-writable
    /// decree directory. The file is **never executed and never consumed**
    /// (it has no trusted identity), and it is left in place for the
    /// operator: quarantining it would require a write the daemon is not
    /// entitled to make on someone else's file.
    ForeignProvenance,
}

impl RejectReason {
    /// Every reason in declaration order.
    pub const ALL: &'static [RejectReason] = &[
        RejectReason::ReplayMismatch,
        RejectReason::UnsupportedVersion,
        RejectReason::Oversize,
        RejectReason::Malformed,
        RejectReason::InvalidTarget,
        RejectReason::UnknownSlot,
        RejectReason::ExecutionFailed,
        RejectReason::Deferred,
        RejectReason::OutOfOrder,
        RejectReason::ForeignProvenance,
    ];

    /// The stable wire token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::ReplayMismatch => "replay_mismatch",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Oversize => "oversize",
            Self::Malformed => "malformed",
            Self::InvalidTarget => "invalid_target",
            Self::UnknownSlot => "unknown_slot",
            Self::ExecutionFailed => "execution_failed",
            Self::Deferred => "deferred",
            Self::OutOfOrder => "out_of_order",
            Self::ForeignProvenance => "foreign_provenance",
        }
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// Outcome recorded in an acknowledgment: executed (`done`) or refused with a
/// typed reason (`rejected`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AckResult {
    /// Executed successfully — including an idempotent re-acknowledgment of
    /// an already-executed identical decree (ADR-0026 cl. 4).
    Done,
    /// Refused before/while executing, with a typed reason.
    Rejected {
        /// Stable rejection token.
        reason: RejectReason,
    },
}

/// The daemon-written acknowledgment `keep/census/manor/home/decrees/
/// <decree_id>.ack` (ADR-0026 cl. 5). Its absence after
/// `config.json`→`mayor.ack_timeout_ms` is what wakes the Mayor with a
/// [`DecreeKind`]less `decree_ack_timeout` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecreeAck {
    /// Must equal [`ACK_VERSION`].
    pub ack_version: u32,
    /// The decree this acknowledges.
    pub decree_id: DecreeId,
    /// Outcome of the attempt.
    pub result: AckResult,
}

impl DecreeAck {
    /// A successful acknowledgment.
    #[must_use]
    pub fn done(id: &DecreeId) -> Self {
        Self {
            ack_version: ACK_VERSION,
            decree_id: id.clone(),
            result: AckResult::Done,
        }
    }

    /// A refusal acknowledgment.
    #[must_use]
    pub fn rejected(id: &DecreeId, reason: RejectReason) -> Self {
        Self {
            ack_version: ACK_VERSION,
            decree_id: id.clone(),
            result: AckResult::Rejected { reason },
        }
    }

    /// Canonical rendering (sorted keys, pretty, trailing newline).
    ///
    /// # Errors
    ///
    /// [`DecreeError::Schema`] (cannot fail for validated values).
    pub fn render(&self) -> Result<String, DecreeError> {
        let value = serde_json::to_value(self).map_err(|e| DecreeError::Schema(e.to_string()))?;
        let mut out =
            serde_json::to_string_pretty(&value).map_err(|e| DecreeError::Schema(e.to_string()))?;
        out.push('\n');
        Ok(out)
    }

    /// Parse an acknowledgment file's contents: size cap → schema → version.
    ///
    /// # Errors
    ///
    /// [`DecreeError::Oversize`], [`DecreeError::Schema`], or
    /// [`DecreeError::UnsupportedVersion`].
    pub fn parse(contents: &str) -> Result<Self, DecreeError> {
        if contents.len() > MAX_ACK_BYTES {
            return Err(DecreeError::Oversize {
                bytes: contents.len(),
            });
        }
        let ack: Self =
            serde_json::from_str(contents).map_err(|e| DecreeError::Schema(e.to_string()))?;
        if ack.ack_version != ACK_VERSION {
            return Err(DecreeError::UnsupportedVersion(ack.ack_version));
        }
        Ok(ack)
    }
}

// ---------------------------------------------------------------------------
// Escalation notice (ADR-0029 cl. 4)
// ---------------------------------------------------------------------------

/// The daemon-written human escalation notice
/// `keep/census/manor/escalations/<decree_id>.json` — what an
/// `escalate_to_human` decree produces (SPEC §7). The Mayor may write only
/// `decrees/`, so the *daemon* files this notice in the operator-facing
/// escalation inbox; Sprint 08's CLI reads exactly this file.
///
/// It is a pure type: rendering/parsing are string operations, the write
/// (temp + fsync + rename) belongs to `hamlet-daemon::decrees`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalationNotice {
    /// Must equal [`ESCALATION_VERSION`].
    pub escalation_version: u32,
    /// The `escalate_to_human` decree this answers.
    pub decree_id: DecreeId,
    /// The decree's evidence labels, re-resolved and re-rendered as
    /// village-relative paths (1..=[`MAX_EVIDENCE_REFS`]).
    pub evidence_refs: Vec<TreeRelPath>,
    /// Unix milliseconds the daemon filed the notice — **lineage data
    /// only** (CONVENTIONS §3 r4): never an input to any state derivation.
    pub recorded_unix_ms: u64,
    /// The Mayor's single-line human summary, verbatim.
    pub summary: String,
}

impl EscalationNotice {
    /// Canonical rendering (sorted keys, pretty, trailing newline).
    ///
    /// # Errors
    ///
    /// [`DecreeError::Schema`] (cannot fail for validated values).
    pub fn render(&self) -> Result<String, DecreeError> {
        let value = serde_json::to_value(self).map_err(|e| DecreeError::Schema(e.to_string()))?;
        let mut out =
            serde_json::to_string_pretty(&value).map_err(|e| DecreeError::Schema(e.to_string()))?;
        out.push('\n');
        Ok(out)
    }

    /// Parse an escalation notice file: size cap → schema → version →
    /// summary shape.
    ///
    /// # Errors
    ///
    /// [`DecreeError::Oversize`], [`DecreeError::Schema`],
    /// [`DecreeError::UnsupportedVersion`], or the summary's
    /// [`DecreeError::InvalidText`].
    pub fn parse(contents: &str) -> Result<Self, DecreeError> {
        if contents.len() > MAX_ESCALATION_BYTES {
            return Err(DecreeError::Oversize {
                bytes: contents.len(),
            });
        }
        let notice: Self =
            serde_json::from_str(contents).map_err(|e| DecreeError::Schema(e.to_string()))?;
        if notice.escalation_version != ESCALATION_VERSION {
            return Err(DecreeError::UnsupportedVersion(notice.escalation_version));
        }
        check_single_line(&notice.summary, MAX_SUMMARY_LEN, "summary")?;
        if notice.evidence_refs.is_empty() {
            return Err(DecreeError::EmptyEvidence);
        }
        if notice.evidence_refs.len() > MAX_EVIDENCE_REFS {
            return Err(DecreeError::TooManyEvidence {
                count: notice.evidence_refs.len(),
            });
        }
        Ok(notice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::sanitize_name;

    fn name(s: &str) -> SafeName {
        sanitize_name(s).expect("test name must sanitize")
    }

    fn path(s: &str) -> TreeRelPath {
        TreeRelPath::parse(s).expect("test path must validate")
    }

    /// A neutral placeholder id: field-level tests re-derive the real id via
    /// `stamp`, so the placeholder's digest never matters.
    fn stub_id() -> DecreeId {
        DecreeId::new(1, "0000000000000000").unwrap()
    }

    /// One decree of every variant, unstamped — the vocabulary table.
    fn corpus() -> Vec<Decree> {
        vec![
            Decree::ProvisionAgent {
                blueprint_ref: name("village-alpine"),
                commission: Some(3),
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                register_ref: name("scout"),
            },
            Decree::HibernateAgent {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                uid: 1042,
            },
            Decree::ReassignTask {
                commission: 3,
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                entry: name("t3"),
                uid: 1042,
            },
            Decree::SteerSession {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                directive: "stop refactoring\nwrite the test first".to_string(),
                uid: 1042,
            },
            Decree::EscalateToHuman {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                evidence_refs: vec![path("courier/freight/.quarantine/exchange.json")],
                summary: "freight ACL violation by slot 1042 needs an operator".to_string(),
            },
            Decree::ApprovePromotion {
                artifact_name: name("report.md"),
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                freight_path: path("courier/freight/exchange/report.md"),
            },
            Decree::RejectPromotion {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                freight_path: path("courier/freight/exchange/report.md"),
                reason: "report does not satisfy the commission goals".to_string(),
            },
        ]
    }

    /// Stamped corpus (each body carries its own derived digest).
    fn stamped_corpus() -> Vec<Decree> {
        corpus().into_iter().map(|d| d.stamp(7).unwrap()).collect()
    }

    // --- identity -------------------------------------------------------

    /// Committed golden vector: the canonical `decree_id` token, and that it
    /// is a valid [`SafeName`] file stem (ADR-0026 cl. 2).
    #[test]
    fn decree_id_render_golden() {
        let id = DecreeId::new(7, "a1b2c3d4e5f60718").unwrap();
        assert_eq!(id.render(), "d-000000000007-a1b2c3d4e5f60718");
        assert_eq!(id.seq(), 7);
        assert_eq!(id.digest(), "a1b2c3d4e5f60718");
        assert_eq!(
            DecreeId::parse("d-000000000007-a1b2c3d4e5f60718").unwrap(),
            id
        );
        assert_eq!(
            id.decree_file_name().unwrap().as_str(),
            "d-000000000007-a1b2c3d4e5f60718.json"
        );
        assert_eq!(
            id.ack_file_name().unwrap().as_str(),
            "d-000000000007-a1b2c3d4e5f60718.ack"
        );
        // the file names are single-extension sanitizer-clean names
        for candidate in [
            "d-000000000007-a1b2c3d4e5f60718.json",
            "d-000000000007-a1b2c3d4e5f60718.ack",
        ] {
            assert!(sanitize_name(candidate).is_ok(), "{candidate}");
        }
        // and parse back
        assert_eq!(
            decree_id_from_file_name("d-000000000007-a1b2c3d4e5f60718.json").unwrap(),
            id
        );
    }

    /// Rejection table for malformed ids/filenames — golden.
    #[test]
    fn id_rejection_table_golden() {
        let cases = [
            "",
            "d-",
            "d-000000000007",
            "x-000000000007-a1b2c3d4e5f60718",
            "d-7-a1b2c3d4e5f60718",
            "d-00000000000a-a1b2c3d4e5f60718",
            "d-000000000007-a1b2c3d4e5f6071",
            "d-000000000007-a1b2c3d4e5f607188",
            "d-000000000007-A1B2C3D4E5F60718",
            "d-000000000007-a1b2c3d4e5f6071g",
            "d-000000000007-a1b2c3d4e5f60718.json",
            "d-000000000007--a1b2c3d4e5f60718",
            "../../etc/passwd",
        ];
        for case in cases {
            assert!(DecreeId::parse(case).is_err(), "{case:?} must be refused");
        }
        // seq 0 is never a valid sequence (1-based counter).
        assert!(DecreeId::new(0, "a1b2c3d4e5f60718").is_err());
        assert!(is_decree_file_name("d-000000000007-a1b2c3d4e5f60718.json"));
        for not_a_decree in [
            "d-000000000007-a1b2c3d4e5f60718.ack",
            "notes.json",
            ".d-000000000007-a1b2c3d4e5f60718.json",
            "d-000000000007-a1b2c3d4e5f60718.JSON",
            "d-000000000007.json",
        ] {
            assert!(!is_decree_file_name(not_a_decree), "{not_a_decree}");
        }
        assert!(decree_id_from_file_name("x.json").is_err());
    }

    /// Monotonicity is expressed by the zero-padded seq: lexicographic file
    // order equals numeric order (ADR-0026 cl. 3).
    #[test]
    fn file_name_order_is_sequence_order() {
        let mut names: Vec<String> = (1..=12)
            .map(|seq| {
                DecreeId::new(seq, "0000000000000000")
                    .unwrap()
                    .decree_file_name()
                    .unwrap()
                    .as_str()
                    .to_owned()
            })
            .collect();
        let sorted = {
            let mut copy = names.clone();
            copy.sort();
            copy
        };
        let by_seq = {
            let mut copy = names.clone();
            copy.sort_by_key(|n| {
                DecreeId::parse(&n[..n.len() - ".json".len()])
                    .unwrap()
                    .seq()
            });
            copy
        };
        assert_eq!(names.len(), 12);
        names.dedup();
        assert_eq!(sorted, by_seq, "lexicographic order must equal seq order");
    }

    /// Committed golden vector: the digest input of a provision decree — the
    /// exact bytes `decree_id` is derived over.
    #[test]
    fn digest_input_golden() {
        let decree = Decree::ProvisionAgent {
            blueprint_ref: name("village-alpine"),
            commission: Some(3),
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            register_ref: name("scout"),
        };
        assert_eq!(
            decree.digest_input(),
            r#"{"blueprint_ref":"village-alpine","commission":3,"decree_type":"provision_agent","decree_version":1,"register_ref":"scout"}"#
        );
        // The stamped id is the derived digest — content-derived identity.
        let stamped = decree.stamp(7).unwrap();
        assert_eq!(stamped.id().digest(), stamped.digest());
        assert_eq!(stamped.validate(), Ok(()));
    }

    /// Every variant round-trips byte-exact through render → parse, and the
    /// stamped id survives (replay identity is stable).
    #[test]
    fn corpus_render_parse_roundtrip_golden() {
        for decree in stamped_corpus() {
            let rendered = decree.render().unwrap();
            let parsed = Decree::parse(&rendered).unwrap();
            assert_eq!(parsed, decree, "{}", decree.kind());
            assert_eq!(parsed.render().unwrap(), rendered);
            assert_eq!(parsed.id(), decree.id());
            // validate() passes on the corpus
            decree.validate().unwrap();
        }
        assert_eq!(stamped_corpus().len(), DecreeKind::ALL.len());
    }

    /// Committed golden vector: exact canonical bytes of one decree file.
    #[test]
    fn decree_render_golden() {
        let decree = Decree::HibernateAgent {
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            uid: 1042,
        }
        .stamp(7)
        .unwrap();
        let expected = "\
{
  \"decree_id\": \"d-000000000007-c0225c3b8fabf543\",
  \"decree_type\": \"hibernate_agent\",
  \"decree_version\": 1,
  \"uid\": 1042
}
";
        // The digest is content-derived; pin the exact bytes (if the FNV
        // input ever changes, this golden is the tripwire).
        assert_eq!(decree.render().unwrap(), expected);
        assert_eq!(Decree::parse(expected).unwrap(), decree);
    }

    /// Field order in the rendered document is declaration-independent: a
    /// decree with the same content always renders the same bytes
    /// (CONVENTIONS §3 r3).
    #[test]
    fn render_is_canonical_regardless_of_declaration_order() {
        let a = Decree::ProvisionAgent {
            blueprint_ref: name("village-alpine"),
            commission: Some(3),
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            register_ref: name("scout"),
        };
        let b = Decree::ProvisionAgent {
            register_ref: name("scout"),
            decree_version: DECREE_VERSION,
            decree_id: stub_id(),
            commission: Some(3),
            blueprint_ref: name("village-alpine"),
        };
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.render().unwrap(), b.render().unwrap());
        // and the tag key sorts among the payload keys
        assert!(
            a.render().unwrap().find("\"decree_type\"").unwrap()
                < a.render().unwrap().find("\"decree_version\"").unwrap()
        );
    }

    /// Committed golden vector: exact canonical bytes of one acknowledgment.
    #[test]
    fn ack_render_golden() {
        let id = DecreeId::new(7, "c0225c3b8fabf543").unwrap();
        assert_eq!(
            DecreeAck::done(&id).render().unwrap(),
            "\
{
  \"ack_version\": 1,
  \"decree_id\": \"d-000000000007-c0225c3b8fabf543\",
  \"result\": {
    \"outcome\": \"done\"
  }
}
"
        );
        assert_eq!(
            DecreeAck::rejected(&id, RejectReason::ReplayMismatch)
                .render()
                .unwrap(),
            "\
{
  \"ack_version\": 1,
  \"decree_id\": \"d-000000000007-c0225c3b8fabf543\",
  \"result\": {
    \"outcome\": \"rejected\",
    \"reason\": \"replay_mismatch\"
  }
}
"
        );
        let ack = DecreeAck::rejected(&id, RejectReason::UnknownSlot);
        assert_eq!(DecreeAck::parse(&ack.render().unwrap()).unwrap(), ack);
        assert_eq!(
            DecreeAck::done(&id)
                .decree_id
                .ack_file_name()
                .unwrap()
                .as_str(),
            "d-000000000007-c0225c3b8fabf543.ack"
        );
        // every reason token is stable and round-trips
        for reason in RejectReason::ALL {
            let ack = DecreeAck::rejected(&id, *reason);
            let parsed = DecreeAck::parse(&ack.render().unwrap()).unwrap();
            assert_eq!(parsed.result, ack.result);
            assert!(!reason.token().is_empty());
        }
    }

    /// The rejection matrix — schema violations are refused, never guessed
    /// (golden table).
    #[test]
    fn parse_rejection_matrix_golden() {
        let stamped = stamped_corpus();
        let base = Decree::HibernateAgent {
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            uid: 1042,
        }
        .stamp(7)
        .unwrap();
        let good = base.render().unwrap();

        // oversize
        let huge = format!("{}{}", good, "x".repeat(MAX_DECREE_BYTES));
        assert_eq!(
            Decree::parse(&huge),
            Err(DecreeError::Oversize { bytes: huge.len() })
        );
        // malformed JSON
        assert!(matches!(Decree::parse("{"), Err(DecreeError::Schema(_))));
        // unknown field is refused, not dropped
        let unknown = good.replace("\"uid\": 1042", "\"uid\": 1042, \"extra\": 1");
        assert!(matches!(
            Decree::parse(&unknown),
            Err(DecreeError::Schema(_))
        ));
        // unknown decree_type
        let unknown_kind = good.replace("hibernate_agent", "decree_everything");
        assert!(matches!(
            Decree::parse(&unknown_kind),
            Err(DecreeError::Schema(_))
        ));
        // wrong version
        let version = good.replace("\"decree_version\": 1", "\"decree_version\": 2");
        assert_eq!(
            Decree::parse(&version),
            Err(DecreeError::UnsupportedVersion(2))
        );
        // tampered body vs stamped digest
        let tampered = good.replace("\"uid\": 1042", "\"uid\": 1043");
        assert!(matches!(
            Decree::parse(&tampered),
            Err(DecreeError::DigestMismatch { .. })
        ));
        // tampered id (digest rewritten to another body's)
        let other = &stamped[0];
        let swapped = good.replace(base.id().digest(), other.id().digest());
        assert!(matches!(
            Decree::parse(&swapped),
            Err(DecreeError::DigestMismatch { .. })
        ));
        // ack version gate + oversize
        let ack = DecreeAck::done(base.id()).render().unwrap();
        let ack_v2 = ack.replace("\"ack_version\": 1", "\"ack_version\": 9");
        assert_eq!(
            DecreeAck::parse(&ack_v2),
            Err(DecreeError::UnsupportedVersion(9))
        );
        assert!(matches!(
            DecreeAck::parse(&format!("{ack}{}", "x".repeat(MAX_ACK_BYTES))),
            Err(DecreeError::Oversize { .. })
        ));
        assert!(matches!(
            DecreeAck::parse("{\"ack_version\":1}"),
            Err(DecreeError::Schema(_))
        ));
    }

    /// Field-level validation matrix: uid ranges, commission bounds, text
    /// bounds, and freight-path region (golden).
    #[test]
    fn field_validation_matrix_golden() {
        let stamp = |d: Decree| d.stamp(7).unwrap();

        // uid outside the slot policy (SPEC §4)
        assert_eq!(
            stamp(Decree::HibernateAgent {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                uid: 4242,
            })
            .validate(),
            Err(DecreeError::InvalidUid(4242))
        );
        // commission 0 is not a ledger commission
        assert_eq!(
            stamp(Decree::ReassignTask {
                commission: 0,
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                entry: name("t1"),
                uid: 1001,
            })
            .validate(),
            Err(DecreeError::InvalidCommission)
        );
        assert_eq!(
            stamp(Decree::ProvisionAgent {
                blueprint_ref: name("village-alpine"),
                commission: Some(0),
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                register_ref: name("scout"),
            })
            .validate(),
            Err(DecreeError::InvalidCommission)
        );
        // freight path must be under courier/freight/ and address a file
        for bad in [
            "courier/mail/1001/x.mail",
            "keep/vault/report.md",
            "courier/freight",
            "courier/freight/",
        ] {
            let Ok(parsed) = TreeRelPath::parse(bad) else {
                continue;
            };
            assert_eq!(
                stamp(Decree::ApprovePromotion {
                    artifact_name: name("report.md"),
                    decree_id: stub_id(),
                    decree_version: DECREE_VERSION,
                    freight_path: parsed,
                })
                .validate(),
                Err(DecreeError::PathOutOfRegion {
                    path: bad.trim_end_matches('/').to_owned()
                }),
                "{bad}"
            );
        }
        // escalation evidence rules
        assert_eq!(
            stamp(Decree::EscalateToHuman {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                evidence_refs: vec![],
                summary: "help".to_string(),
            })
            .validate(),
            Err(DecreeError::EmptyEvidence)
        );
        assert_eq!(
            stamp(Decree::EscalateToHuman {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                evidence_refs: (0..=MAX_EVIDENCE_REFS)
                    .map(|i| path(&format!("keep/census/parcel10{i}/DEED.md")))
                    .collect(),
                summary: "help".to_string(),
            })
            .validate(),
            Err(DecreeError::TooManyEvidence {
                count: MAX_EVIDENCE_REFS + 1
            })
        );
        // text bounds
        assert_eq!(
            stamp(Decree::RejectPromotion {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                freight_path: path("courier/freight/exchange/report.md"),
                reason: "line one\nline two".to_string(),
            })
            .validate(),
            Err(DecreeError::InvalidText {
                field: "reason",
                reason: "must not contain control characters"
            })
        );
        assert_eq!(
            stamp(Decree::SteerSession {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                directive: "windows\r\nlines".to_string(),
                uid: 1001,
            })
            .validate(),
            Err(DecreeError::InvalidText {
                field: "directive",
                reason: "must not contain CR (LF-only contract)"
            })
        );
        assert_eq!(
            stamp(Decree::EscalateToHuman {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                evidence_refs: vec![path("charter.json")],
                summary: "x".repeat(MAX_SUMMARY_LEN + 1),
            })
            .validate(),
            Err(DecreeError::InvalidText {
                field: "summary",
                reason: "too long"
            })
        );
        assert_eq!(
            stamp(Decree::SteerSession {
                decree_id: stub_id(),
                decree_version: DECREE_VERSION,
                directive: " ".to_string(),
                uid: 1001,
            })
            .validate(),
            Err(DecreeError::InvalidText {
                field: "directive",
                reason: "must not be blank"
            })
        );
    }

    /// Village-relative path labels are traversal-proof and join only under
    /// the given root (SPEC §2 name-safety).
    #[test]
    fn tree_rel_path_is_traversal_proof() {
        for bad in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            ".",
            "a//b",
            "trailing/",
            "back\\slash",
            "nul\0byte",
            "-leading",
            "with space",
            "unicode\u{e9}",
        ] {
            assert!(TreeRelPath::parse(bad).is_err(), "{bad:?} must be refused");
        }
        // depth cap
        let deep = (0..=MAX_PATH_COMPONENTS)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        assert!(matches!(
            TreeRelPath::parse(&deep),
            Err(DecreeError::InvalidPath { .. })
        ));
        // component length cap
        let long = "a".repeat(MAX_PATH_COMPONENT_LEN + 1);
        assert!(TreeRelPath::parse(&long).is_err());
        let ok = path("courier/freight/.quarantine/exchange.json");
        assert_eq!(
            ok.components(),
            ["courier", "freight", ".quarantine", "exchange.json"]
        );
        assert!(ok.starts_with("courier/freight"));
        assert!(!ok.starts_with("courier/freight/deeper/only"));
        assert!(path("courier/freight").starts_with("courier/freight"));
        // hidden components are legal labels (quarantine flags are evidence)
        let joined = ok.join_under(Path::new("/v/.hamlet")).unwrap();
        assert_eq!(
            joined,
            PathBuf::from("/v/.hamlet/courier/freight/.quarantine/exchange.json")
        );
        assert!(joined.starts_with("/v/.hamlet"));
        // serde validates on the way in
        assert!(serde_json::from_str::<TreeRelPath>("\"../evil\"").is_err());
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            "\"courier/freight/.quarantine/exchange.json\""
        );
    }

    /// Replay semantics are pure id algebra: same body + same seq ⇒ same id;
    /// different body ⇒ different digest (so the charter can refuse it).
    #[test]
    fn replay_identity_algebra() {
        let base = stamped_corpus();
        let a = Decree::HibernateAgent {
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            uid: 1042,
        }
        .stamp(7)
        .unwrap();
        let a_again = Decree::HibernateAgent {
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            uid: 1042,
        }
        .stamp(7)
        .unwrap();
        let b = Decree::HibernateAgent {
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            uid: 1001,
        }
        .stamp(7)
        .unwrap();
        assert_eq!(a.id(), a_again.id(), "identical content ⇒ identical id");
        assert_ne!(a.id(), b.id(), "different content ⇒ different id");
        // Re-stamping the same body at another sequence is a *new* decree.
        let a_at_8 = Decree::HibernateAgent {
            decree_id: stub_id(),
            decree_version: DECREE_VERSION,
            uid: 1042,
        }
        .stamp(8)
        .unwrap();
        assert_eq!(a_at_8.id().seq(), 8);
        assert_eq!(a_at_8.id().digest(), a.id().digest());
        assert_eq!(base.len(), 7);
    }

    /// Digest is dependency-free and stable: pinned vectors (golden).
    #[test]
    fn content_digest_golden() {
        assert_eq!(content_digest(b""), "cbf29ce484222325");
        assert_eq!(content_digest(b"a"), "af63dc4c8601ec8c");
        assert_eq!(content_digest(b"foobar"), "85944171f73967e8");
        // FNV-1a/64 test vector for "hello"
        assert_eq!(content_digest(b"hello"), "a430d84680aabd0b");
    }

    /// Vocabulary tokens are the closed, stable set (ADR-0026 cl. 1).
    #[test]
    fn kind_tokens_golden() {
        assert_eq!(
            DecreeKind::ALL
                .iter()
                .map(|k| k.token())
                .collect::<Vec<_>>(),
            vec![
                "approve_promotion",
                "hibernate_agent",
                "escalate_to_human",
                "provision_agent",
                "reject_promotion",
                "reassign_task",
                "steer_session",
            ]
        );
        for decree in stamped_corpus() {
            let rendered = decree.render().unwrap();
            assert!(
                rendered.contains(&format!("\"decree_type\": \"{}\"", decree.kind())),
                "{rendered}"
            );
        }
    }

    /// The escalation notice is a daemon-written artifact of an
    /// `escalate_to_human` decree (ADR-0029 cl. 4): canonical bytes, strict
    /// parse, and the same typed refusals as the decree it answers.
    #[test]
    fn escalation_notice_render_parse_golden() {
        let id = DecreeId::new(4, "0123456789abcdef").unwrap();
        let notice = EscalationNotice {
            escalation_version: ESCALATION_VERSION,
            decree_id: id.clone(),
            evidence_refs: vec![path("courier/freight/exchange/report.md")],
            recorded_unix_ms: 1_700_000_000_000,
            summary: "freight ACL violation needs an operator".to_owned(),
        };
        let expected = "\
{
  \"decree_id\": \"d-000000000004-0123456789abcdef\",
  \"escalation_version\": 1,
  \"evidence_refs\": [
    \"courier/freight/exchange/report.md\"
  ],
  \"recorded_unix_ms\": 1700000000000,
  \"summary\": \"freight ACL violation needs an operator\"
}
";
        assert_eq!(notice.render().unwrap(), expected);
        assert_eq!(EscalationNotice::parse(expected).unwrap(), notice);

        // rejections: version, empty evidence, control chars, oversize
        let mut bad_version = notice.clone();
        bad_version.escalation_version = 2;
        assert!(matches!(
            EscalationNotice::parse(&bad_version.render().unwrap()),
            Err(DecreeError::UnsupportedVersion(2))
        ));
        let mut no_evidence = notice.clone();
        no_evidence.evidence_refs = Vec::new();
        assert!(matches!(
            EscalationNotice::parse(&no_evidence.render().unwrap()),
            Err(DecreeError::EmptyEvidence)
        ));
        let mut multiline = notice.clone();
        multiline.summary = "line one\nline two".to_owned();
        assert!(matches!(
            EscalationNotice::parse(&multiline.render().unwrap()),
            Err(DecreeError::InvalidText {
                field: "summary",
                ..
            })
        ));
        assert!(matches!(
            EscalationNotice::parse(&"x".repeat(MAX_ESCALATION_BYTES + 1)),
            Err(DecreeError::Oversize { .. })
        ));
        // unknown fields are corruption, never silently dropped
        let extra = expected.replace("\"summary\"", "\"extra\": 1, \"summary\"");
        assert!(matches!(
            EscalationNotice::parse(&extra),
            Err(DecreeError::Schema(_))
        ));
    }
}
