//! Stasis schemas, census chain-of-title, and resource policy types (SPEC §10, §14; ADR-0039, ADR-0040).
//!
//! Provides normative schemas and pure state derivation for:
//! 1. **Patron record schema**: `tavern/patrons/PROPRIETOR<name>/` layout,
//!    `PROPRIETOR.md` metadata frontmatter + intelligence profile body ([`PatronDoc`]),
//!    and `home.uid.bak/` integrity manifest ([`BackupManifest`]).
//! 2. **TITLE.md grammar**: append-only chain-of-title lineage ([`TitleDoc`], [`TitleEntry`])
//!    with daemon-stamped entries (timestamps as lineage data, CONVENTIONS §3.4).
//! 3. **Dual-state convergence**: independent tracking of environment promotions
//!    (DEED → blueprint) and intelligence promotions (PROPRIETOR → patron)
//!    ([`DualConvergenceReport`], [`CommissionConvergenceSummary`]).
//! 4. **Resource policy & candidate ranking**: container limits, pressure detection,
//!    candidate selection ranking ([`rank_candidates`]), and OOM policy evaluation
//!    ([`evaluate_oom_policy`]).
//!
//! All types are pure, side-effect-free, and deterministic (ADR-0001 purity seam).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::decree::content_digest;
use crate::sanitize::{sanitize_name, SafeName};
use crate::slots::{FIRST_AGENT_UID, LAST_AGENT_UID};

// ---------------------------------------------------------------------------
// Constants & Limits (ADR-0039, ADR-0040)
// ---------------------------------------------------------------------------

/// Schema version for patron metadata frontmatter (ADR-0039 cl. 2).
pub const PATRON_SCHEMA_VERSION: u32 = 1;

/// Schema version for backup integrity manifest (ADR-0039 cl. 3).
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Maximum frontmatter size in bytes (4 KiB).
pub const MAX_PROPRIETOR_FRONTMATTER_BYTES: usize = 4 * 1024;

/// Maximum intelligence profile body size in bytes (64 KiB).
pub const MAX_PROPRIETOR_BODY_BYTES: usize = 64 * 1024;

/// Maximum whole PROPRIETOR.md size in bytes.
pub const MAX_PROPRIETOR_TOTAL_BYTES: usize =
    MAX_PROPRIETOR_FRONTMATTER_BYTES + MAX_PROPRIETOR_BODY_BYTES + 256;

/// Maximum TITLE.md file size accepted by [`TitleDoc::parse`] (256 KiB).
pub const MAX_TITLE_BYTES: usize = 256 * 1024;

/// Maximum single line length in TITLE.md (4 KiB).
pub const MAX_TITLE_LINE_BYTES: usize = 4 * 1024;

/// Maximum backup manifest JSON size in bytes (1 MiB).
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Part 1: Patron Record & PROPRIETOR.md Metadata
// ---------------------------------------------------------------------------

/// Machine-parseable metadata block from `PROPRIETOR.md` frontmatter (ADR-0039 cl. 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatronFrontmatter {
    /// Patron identifier (SafeName).
    pub patron: SafeName,
    /// Register archetype reference from `guild/register/`.
    pub register: SafeName,
    /// Environment blueprint reference from `guild/blueprints/`.
    pub blueprint: SafeName,
    /// Version of the environment blueprint.
    pub blueprint_version: u32,
    /// Historical slot UID from which this patron was hibernated (1001..=2000).
    pub source_uid: u32,
    /// Last commission served, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commission: Option<u64>,
    /// Capability and skill tags used for quick-awakening matching.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tags: BTreeSet<SafeName>,
    /// Lineage timestamp (epoch seconds, never a state input; CONVENTIONS §3.4).
    pub created_at: u64,
    /// Schema version of this metadata block.
    pub stasis_version: u32,
}

/// A parsed `PROPRIETOR.md` document consisting of machine-parseable
/// frontmatter and a freeform intelligence profile body (ADR-0039 cl. 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatronDoc {
    /// Validated metadata frontmatter.
    pub frontmatter: PatronFrontmatter,
    /// Freeform markdown intelligence profile.
    pub body: String,
}

/// Errors occurring during patron document parsing or validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatronError {
    /// Document exceeds byte limit.
    TooLarge { len: usize, max: usize },
    /// CRLF / CR line endings rejected (LF-only boundary; ADR-0019 cl. 1).
    BadLineEnding { line: usize },
    /// Missing opening `---` fence on line 1.
    MissingFrontmatterStart,
    /// Missing closing `---` fence for frontmatter.
    MissingFrontmatterEnd,
    /// Frontmatter exceeds byte limit.
    FrontmatterTooLarge { len: usize },
    /// Body exceeds byte limit.
    BodyTooLarge { len: usize },
    /// Frontmatter key/value syntax error.
    InvalidFrontmatter { line: usize, detail: String },
    /// Schema validation failed for a specific field.
    InvalidField { field: &'static str, reason: String },
}

impl fmt::Display for PatronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => {
                write!(f, "PROPRIETOR document too large: {len} bytes (max {max})")
            }
            Self::BadLineEnding { line } => {
                write!(f, "line {line}: carriage return rejected (LF-only text)")
            }
            Self::MissingFrontmatterStart => {
                write!(f, "line 1: missing opening '---' frontmatter fence")
            }
            Self::MissingFrontmatterEnd => {
                write!(f, "missing closing '---' frontmatter fence")
            }
            Self::FrontmatterTooLarge { len } => {
                write!(
                    f,
                    "frontmatter too large: {len} bytes (max {MAX_PROPRIETOR_FRONTMATTER_BYTES})"
                )
            }
            Self::BodyTooLarge { len } => {
                write!(
                    f,
                    "body too large: {len} bytes (max {MAX_PROPRIETOR_BODY_BYTES})"
                )
            }
            Self::InvalidFrontmatter { line, detail } => {
                write!(f, "line {line}: invalid frontmatter: {detail}")
            }
            Self::InvalidField { field, reason } => {
                write!(f, "invalid field '{field}': {reason}")
            }
        }
    }
}

impl std::error::Error for PatronError {}

impl PatronDoc {
    /// Parse a `PROPRIETOR.md` text document (ADR-0039 cl. 2).
    ///
    /// # Errors
    ///
    /// [`PatronError`] on size violations, CRLF line endings, malformed
    /// frontmatter, or invalid field values.
    pub fn parse(text: &str) -> Result<Self, PatronError> {
        if text.len() > MAX_PROPRIETOR_TOTAL_BYTES {
            return Err(PatronError::TooLarge {
                len: text.len(),
                max: MAX_PROPRIETOR_TOTAL_BYTES,
            });
        }
        if let Some(pos) = text.find('\r') {
            let line = text[..pos].chars().filter(|&c| c == '\n').count() + 1;
            return Err(PatronError::BadLineEnding { line });
        }

        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() || lines[0].trim() != "---" {
            return Err(PatronError::MissingFrontmatterStart);
        }

        let mut end_idx = None;
        for (i, line) in lines.iter().enumerate().skip(1) {
            if line.trim() == "---" {
                end_idx = Some(i);
                break;
            }
        }

        let end_idx = end_idx.ok_or(PatronError::MissingFrontmatterEnd)?;
        let frontmatter_lines = &lines[1..end_idx];
        let frontmatter_text = frontmatter_lines.join("\n");
        if frontmatter_text.len() > MAX_PROPRIETOR_FRONTMATTER_BYTES {
            return Err(PatronError::FrontmatterTooLarge {
                len: frontmatter_text.len(),
            });
        }

        let frontmatter = Self::parse_frontmatter(frontmatter_lines)?;

        let body_lines = &lines[(end_idx + 1)..];
        let body_raw = body_lines.join("\n");
        let body = body_raw.trim_start_matches('\n').to_string();
        if body.len() > MAX_PROPRIETOR_BODY_BYTES {
            return Err(PatronError::BodyTooLarge { len: body.len() });
        }

        Ok(Self { frontmatter, body })
    }

    fn parse_frontmatter(lines: &[&str]) -> Result<PatronFrontmatter, PatronError> {
        let mut patron: Option<SafeName> = None;
        let mut register: Option<SafeName> = None;
        let mut blueprint: Option<SafeName> = None;
        let mut blueprint_version: Option<u32> = None;
        let mut source_uid: Option<u32> = None;
        let mut commission: Option<u64> = None;
        let mut tags = BTreeSet::new();
        let mut created_at: Option<u64> = None;
        let mut stasis_version: Option<u32> = None;

        for (rel_idx, line) in lines.iter().enumerate() {
            let line_num = rel_idx + 2; // 1-based, skipping initial '---'
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let (key, val) =
                trimmed
                    .split_once(':')
                    .ok_or_else(|| PatronError::InvalidFrontmatter {
                        line: line_num,
                        detail: format!("expected 'key: value', got '{trimmed}'"),
                    })?;
            let key = key.trim();
            let val = val.trim();

            match key {
                "patron" => {
                    let name = sanitize_name(val).map_err(|e| PatronError::InvalidField {
                        field: "patron",
                        reason: e.to_string(),
                    })?;
                    patron = Some(name);
                }
                "register" => {
                    let name = sanitize_name(val).map_err(|e| PatronError::InvalidField {
                        field: "register",
                        reason: e.to_string(),
                    })?;
                    register = Some(name);
                }
                "blueprint" => {
                    let name = sanitize_name(val).map_err(|e| PatronError::InvalidField {
                        field: "blueprint",
                        reason: e.to_string(),
                    })?;
                    blueprint = Some(name);
                }
                "blueprint_version" => {
                    let v: u32 = val.parse().map_err(|_| PatronError::InvalidField {
                        field: "blueprint_version",
                        reason: format!("must be a positive integer, got '{val}'"),
                    })?;
                    if v == 0 {
                        return Err(PatronError::InvalidField {
                            field: "blueprint_version",
                            reason: "must be >= 1".to_string(),
                        });
                    }
                    blueprint_version = Some(v);
                }
                "source_uid" => {
                    let uid: u32 = val.parse().map_err(|_| PatronError::InvalidField {
                        field: "source_uid",
                        reason: format!("must be an integer UID, got '{val}'"),
                    })?;
                    if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
                        return Err(PatronError::InvalidField {
                            field: "source_uid",
                            reason: format!(
                                "source_uid {uid} outside subagent slot range {FIRST_AGENT_UID}..={LAST_AGENT_UID}"
                            ),
                        });
                    }
                    source_uid = Some(uid);
                }
                "commission" => {
                    if val != "none" && !val.is_empty() {
                        let comm: u64 = val.parse().map_err(|_| PatronError::InvalidField {
                            field: "commission",
                            reason: format!("must be a positive integer, got '{val}'"),
                        })?;
                        commission = Some(comm);
                    }
                }
                "tags" => {
                    let raw_tags = val.trim_start_matches('[').trim_end_matches(']');
                    for token in raw_tags.split(',') {
                        let token = token.trim().trim_matches('"').trim_matches('\'');
                        if !token.is_empty() {
                            let tag =
                                sanitize_name(token).map_err(|e| PatronError::InvalidField {
                                    field: "tags",
                                    reason: format!("invalid tag '{token}': {e}"),
                                })?;
                            tags.insert(tag);
                        }
                    }
                }
                "created_at" => {
                    let ts: u64 = val.parse().map_err(|_| PatronError::InvalidField {
                        field: "created_at",
                        reason: format!("must be epoch timestamp, got '{val}'"),
                    })?;
                    created_at = Some(ts);
                }
                "stasis_version" => {
                    let v: u32 = val.parse().map_err(|_| PatronError::InvalidField {
                        field: "stasis_version",
                        reason: format!("must be an integer, got '{val}'"),
                    })?;
                    if v != PATRON_SCHEMA_VERSION {
                        return Err(PatronError::InvalidField {
                            field: "stasis_version",
                            reason: format!(
                                "unsupported schema version {v} (expected {PATRON_SCHEMA_VERSION})"
                            ),
                        });
                    }
                    stasis_version = Some(v);
                }
                unknown => {
                    return Err(PatronError::InvalidFrontmatter {
                        line: line_num,
                        detail: format!("unknown frontmatter key '{unknown}'"),
                    });
                }
            }
        }

        let patron = patron.ok_or(PatronError::InvalidField {
            field: "patron",
            reason: "missing required field".to_string(),
        })?;
        let register = register.ok_or(PatronError::InvalidField {
            field: "register",
            reason: "missing required field".to_string(),
        })?;
        let blueprint = blueprint.ok_or(PatronError::InvalidField {
            field: "blueprint",
            reason: "missing required field".to_string(),
        })?;
        let blueprint_version = blueprint_version.ok_or(PatronError::InvalidField {
            field: "blueprint_version",
            reason: "missing required field".to_string(),
        })?;
        let source_uid = source_uid.ok_or(PatronError::InvalidField {
            field: "source_uid",
            reason: "missing required field".to_string(),
        })?;
        let created_at = created_at.ok_or(PatronError::InvalidField {
            field: "created_at",
            reason: "missing required field".to_string(),
        })?;
        let stasis_version = stasis_version.unwrap_or(PATRON_SCHEMA_VERSION);

        Ok(PatronFrontmatter {
            patron,
            register,
            blueprint,
            blueprint_version,
            source_uid,
            commission,
            tags,
            created_at,
            stasis_version,
        })
    }

    /// Render the patron document into canonical, byte-exact markdown (ADR-0039 cl. 2).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str("---\n");
        out.push_str(&format!("patron: {}\n", self.frontmatter.patron));
        out.push_str(&format!("register: {}\n", self.frontmatter.register));
        out.push_str(&format!("blueprint: {}\n", self.frontmatter.blueprint));
        out.push_str(&format!(
            "blueprint_version: {}\n",
            self.frontmatter.blueprint_version
        ));
        out.push_str(&format!("source_uid: {}\n", self.frontmatter.source_uid));
        if let Some(c) = self.frontmatter.commission {
            out.push_str(&format!("commission: {c}\n"));
        }
        let tag_strs: Vec<String> = self
            .frontmatter
            .tags
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect();
        out.push_str(&format!("tags: [{}]\n", tag_strs.join(", ")));
        out.push_str(&format!("created_at: {}\n", self.frontmatter.created_at));
        out.push_str(&format!(
            "stasis_version: {}\n",
            self.frontmatter.stasis_version
        ));
        out.push_str("---\n\n");
        out.push_str(&self.body);
        if !self.body.is_empty() && !self.body.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Quick-awakening matching rule (SPEC §10; ADR-0039): matches if register archetype
    /// and blueprint match, and all required skill tags are present.
    #[must_use]
    pub fn matches_skill_config(
        &self,
        register: &SafeName,
        blueprint: &SafeName,
        required_tags: &[SafeName],
    ) -> bool {
        self.frontmatter.register == *register
            && self.frontmatter.blueprint == *blueprint
            && required_tags
                .iter()
                .all(|t| self.frontmatter.tags.contains(t))
    }
}

// ---------------------------------------------------------------------------
// Part 2: Backup Integrity Manifest
// ---------------------------------------------------------------------------

/// The type of entry in the backup manifest (ADR-0039 cl. 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestEntryType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symlink (note: refused on restore per ADR-0007, but recorded if observed).
    Symlink,
}

impl fmt::Display for ManifestEntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File => f.write_str("file"),
            Self::Directory => f.write_str("directory"),
            Self::Symlink => f.write_str("symlink"),
        }
    }
}

/// One recorded top-level entry in `home.uid.bak/` (ADR-0039 cl. 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Entry type.
    pub entry_type: ManifestEntryType,
    /// Content digest (FNV-1a 16 hex chars or SHA-256).
    pub digest: String,
    /// Size in bytes (for files).
    pub size_bytes: u64,
    /// POSIX file mode permissions (e.g. 0o644, 0o755).
    pub mode: u32,
}

/// Backup manifest recorded at `tavern/patrons/PROPRIETOR<name>/manifest.json` (ADR-0039 cl. 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Manifest schema version (default [`MANIFEST_SCHEMA_VERSION`]).
    pub manifest_version: u32,
    /// Patron identifier.
    pub patron: SafeName,
    /// Source slot UID.
    pub source_uid: u32,
    /// Lineage timestamp.
    pub created_at: u64,
    /// Total bytes across all top-level files in the backup.
    pub total_bytes: u64,
    /// Top-level entries sorted by entry name.
    pub entries: BTreeMap<String, ManifestEntry>,
}

/// Errors occurring during backup manifest validation or parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Malformed JSON payload.
    BadJson(String),
    /// Exceeds maximum size.
    TooLarge { len: usize },
    /// Invalid entry name (contains path separator, null byte, or is dot/dot-dot).
    InvalidEntryName(String),
    /// Verification failure: an entry did not match its recorded manifest record.
    EntryMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    /// An entry present in the backup was missing from the manifest.
    MissingEntry(String),
    /// Schema version mismatch.
    UnsupportedVersion(u32),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadJson(e) => write!(f, "manifest json error: {e}"),
            Self::TooLarge { len } => {
                write!(
                    f,
                    "manifest exceeds cap: {len} bytes (max {MAX_MANIFEST_BYTES})"
                )
            }
            Self::InvalidEntryName(name) => write!(f, "invalid manifest entry name '{name}'"),
            Self::EntryMismatch {
                name,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "entry '{name}' integrity mismatch: expected {expected}, got {actual}"
                )
            }
            Self::MissingEntry(name) => write!(f, "entry '{name}' missing from manifest"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported manifest version {v}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl BackupManifest {
    /// Parse and validate a canonical JSON backup manifest.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] on malformed JSON, invalid entry names, or unsupported schema.
    pub fn parse(json_str: &str) -> Result<Self, ManifestError> {
        if json_str.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                len: json_str.len(),
            });
        }
        let manifest: Self =
            serde_json::from_str(json_str).map_err(|e| ManifestError::BadJson(e.to_string()))?;
        if manifest.manifest_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedVersion(manifest.manifest_version));
        }
        for name in manifest.entries.keys() {
            Self::validate_entry_name(name)?;
        }
        Ok(manifest)
    }

    /// Validate that an entry name is a single, valid directory entry name.
    pub fn validate_entry_name(name: &str) -> Result<(), ManifestError> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
        {
            return Err(ManifestError::InvalidEntryName(name.to_string()));
        }
        Ok(())
    }

    /// Render into canonical, sorted-key pretty JSON with trailing newline.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] if serialization fails.
    pub fn render(&self) -> Result<String, ManifestError> {
        let mut s = serde_json::to_string_pretty(self)
            .map_err(|e| ManifestError::BadJson(e.to_string()))?;
        s.push('\n');
        Ok(s)
    }

    /// Verify a single top-level entry against this manifest.
    ///
    /// # Errors
    ///
    /// [`ManifestError::EntryMismatch`] or [`ManifestError::MissingEntry`] if validation fails.
    pub fn verify_entry(
        &self,
        name: &str,
        entry_type: ManifestEntryType,
        size_bytes: u64,
        digest: &str,
    ) -> Result<(), ManifestError> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| ManifestError::MissingEntry(name.to_string()))?;

        if entry.entry_type != entry_type {
            return Err(ManifestError::EntryMismatch {
                name: name.to_string(),
                expected: format!("type {}", entry.entry_type),
                actual: format!("type {entry_type}"),
            });
        }
        if entry_type == ManifestEntryType::File && entry.size_bytes != size_bytes {
            return Err(ManifestError::EntryMismatch {
                name: name.to_string(),
                expected: format!("size {} bytes", entry.size_bytes),
                actual: format!("size {size_bytes} bytes"),
            });
        }
        if entry.digest != digest {
            return Err(ManifestError::EntryMismatch {
                name: name.to_string(),
                expected: format!("digest {}", entry.digest),
                actual: format!("digest {digest}"),
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Part 3: TITLE.md Grammar & Lineage
// ---------------------------------------------------------------------------

/// The normative entry kinds in `TITLE.md` (ADR-0039 cl. 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleEntryKind {
    /// Upstream player prompt or commission triggering this workspace.
    Trigger,
    /// Parcel slot provisioned for an archetype and blueprint.
    Provision,
    /// Session process started or bumped incarnation.
    SessionIncarnation,
    /// Session crashed.
    Crash,
    /// Out-of-band steering or divine interruption.
    Steer,
    /// Environment, deliverable, or patron promotion event.
    Promotion,
    /// Agent parked into stasis.
    Hibernate,
    /// Agent awakened into a new slot.
    Awaken,
}

impl TitleEntryKind {
    /// Stable wire / grammar token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Provision => "provision",
            Self::SessionIncarnation => "session",
            Self::Crash => "crash",
            Self::Steer => "steer",
            Self::Promotion => "promotion",
            Self::Hibernate => "hibernate",
            Self::Awaken => "awaken",
        }
    }
}

impl fmt::Display for TitleEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// One daemon-stamped entry in `TITLE.md` (ADR-0039 cl. 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleEntry {
    /// Initial trigger lineage.
    Trigger {
        /// Lineage timestamp.
        timestamp: String,
        /// Commission ID if known.
        commission: Option<u64>,
        /// Commission slug if known.
        slug: Option<SafeName>,
        /// Summary trigger prompt or description.
        prompt: String,
    },
    /// Provisioning record.
    Provision {
        /// Lineage timestamp.
        timestamp: String,
        /// Slot UID.
        uid: u32,
        /// Register archetype identity.
        archetype: SafeName,
        /// Blueprint identifier.
        blueprint: SafeName,
        /// Blueprint version.
        version: u32,
    },
    /// Session incarnation event.
    SessionIncarnation {
        /// Lineage timestamp.
        timestamp: String,
        /// Session identifier token (e.g. `s-1001-0001`).
        session_id: String,
        /// Incarnation counter (>= 1).
        incarnation: u32,
    },
    /// Crash lineage entry.
    Crash {
        /// Lineage timestamp.
        timestamp: String,
        /// Session identifier token.
        session_id: String,
        /// Process exit code if available.
        exit_code: Option<i32>,
        /// Consecutive restarts count.
        restarts: u32,
        /// Scrubbed reason / signal.
        reason: String,
    },
    /// Steering directive.
    Steer {
        /// Lineage timestamp.
        timestamp: String,
        /// Steer directive summary.
        directive: String,
    },
    /// Promotion event.
    Promotion {
        /// Lineage timestamp.
        timestamp: String,
        /// Promotion kind (e.g. `deed`, `vault`, `patron`).
        kind: String,
        /// Target artifact or blueprint name.
        target: String,
        /// Status (e.g. `promoted`, `rejected`).
        status: String,
    },
    /// Hibernation stasis event.
    Hibernate {
        /// Lineage timestamp.
        timestamp: String,
        /// Hibernated patron name.
        patron: SafeName,
        /// Hibernation trigger/reason.
        reason: String,
    },
    /// Awakening event.
    Awaken {
        /// Lineage timestamp.
        timestamp: String,
        /// Patron name awakened.
        patron: SafeName,
        /// Commission ID awakened from, if recorded.
        from_commission: Option<u64>,
    },
}

impl TitleEntry {
    /// The entry kind.
    #[must_use]
    pub fn kind(&self) -> TitleEntryKind {
        match self {
            Self::Trigger { .. } => TitleEntryKind::Trigger,
            Self::Provision { .. } => TitleEntryKind::Provision,
            Self::SessionIncarnation { .. } => TitleEntryKind::SessionIncarnation,
            Self::Crash { .. } => TitleEntryKind::Crash,
            Self::Steer { .. } => TitleEntryKind::Steer,
            Self::Promotion { .. } => TitleEntryKind::Promotion,
            Self::Hibernate { .. } => TitleEntryKind::Hibernate,
            Self::Awaken { .. } => TitleEntryKind::Awaken,
        }
    }

    /// The lineage timestamp string.
    #[must_use]
    pub fn timestamp(&self) -> &str {
        match self {
            Self::Trigger { timestamp, .. }
            | Self::Provision { timestamp, .. }
            | Self::SessionIncarnation { timestamp, .. }
            | Self::Crash { timestamp, .. }
            | Self::Steer { timestamp, .. }
            | Self::Promotion { timestamp, .. }
            | Self::Hibernate { timestamp, .. }
            | Self::Awaken { timestamp, .. } => timestamp,
        }
    }

    /// Render this entry as a single appendable bullet line (ADR-0039 cl. 4).
    #[must_use]
    pub fn render_line(&self) -> String {
        match self {
            Self::Trigger {
                timestamp,
                commission,
                slug,
                prompt,
            } => {
                let mut s = format!("- [{timestamp}] trigger");
                if let Some(c) = commission {
                    s.push_str(&format!(" commission={c}"));
                }
                if let Some(slug) = slug {
                    s.push_str(&format!(" slug={slug}"));
                }
                let clean_prompt = prompt
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', " ");
                s.push_str(&format!(" prompt=\"{clean_prompt}\"\n"));
                s
            }
            Self::Provision {
                timestamp,
                uid,
                archetype,
                blueprint,
                version,
            } => {
                format!(
                    "- [{timestamp}] provision uid={uid} archetype={archetype} blueprint={blueprint} version={version}\n"
                )
            }
            Self::SessionIncarnation {
                timestamp,
                session_id,
                incarnation,
            } => {
                format!(
                    "- [{timestamp}] session session_id={session_id} incarnation={incarnation}\n"
                )
            }
            Self::Crash {
                timestamp,
                session_id,
                exit_code,
                restarts,
                reason,
            } => {
                let code_str = match exit_code {
                    Some(c) => c.to_string(),
                    None => "none".to_string(),
                };
                let clean_reason = reason
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', " ");
                format!(
                    "- [{timestamp}] crash session_id={session_id} exit_code={code_str} restarts={restarts} reason=\"{clean_reason}\"\n"
                )
            }
            Self::Steer {
                timestamp,
                directive,
            } => {
                let clean_dir = directive
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', " ");
                format!("- [{timestamp}] steer directive=\"{clean_dir}\"\n")
            }
            Self::Promotion {
                timestamp,
                kind,
                target,
                status,
            } => {
                format!("- [{timestamp}] promotion kind={kind} target={target} status={status}\n")
            }
            Self::Hibernate {
                timestamp,
                patron,
                reason,
            } => {
                let clean_reason = reason
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', " ");
                format!("- [{timestamp}] hibernate patron={patron} reason=\"{clean_reason}\"\n")
            }
            Self::Awaken {
                timestamp,
                patron,
                from_commission,
            } => {
                let mut s = format!("- [{timestamp}] awaken patron={patron}");
                if let Some(fc) = from_commission {
                    s.push_str(&format!(" from_commission={fc}"));
                }
                s.push('\n');
                s
            }
        }
    }
}

/// A parsed `TITLE.md` chain-of-title document (ADR-0039 cl. 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleDoc {
    /// Header line (e.g. `# TITLE — parcel1001` or `# TITLE — manor`).
    pub header: String,
    /// Chronological list of entries.
    pub entries: Vec<TitleEntry>,
}

/// Errors occurring during TITLE.md parsing or validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleError {
    /// Document exceeds byte limit.
    TooLarge { len: usize },
    /// Carriage return rejected (LF-only text boundary).
    BadLineEnding { line: usize },
    /// Missing initial `# TITLE` heading on line 1.
    MissingHeader { line: usize },
    /// Line parse failure.
    ParseError { line: usize, detail: String },
    /// Non-append mutation detected (historical entries modified or deleted).
    NonAppendMutation { detail: String },
}

impl fmt::Display for TitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len } => {
                write!(
                    f,
                    "TITLE document too large: {len} bytes (max {MAX_TITLE_BYTES})"
                )
            }
            Self::BadLineEnding { line } => {
                write!(f, "line {line}: carriage return rejected (LF-only text)")
            }
            Self::MissingHeader { line } => {
                write!(f, "line {line}: missing '# TITLE' header")
            }
            Self::ParseError { line, detail } => {
                write!(f, "line {line}: invalid title entry: {detail}")
            }
            Self::NonAppendMutation { detail } => {
                write!(f, "non-append mutation detected in TITLE.md: {detail}")
            }
        }
    }
}

impl std::error::Error for TitleError {}

impl TitleDoc {
    /// Parse a `TITLE.md` document (ADR-0039 cl. 4).
    ///
    /// # Errors
    ///
    /// [`TitleError`] on size violations, CRLF line endings, missing header,
    /// or unparseable lines.
    pub fn parse(text: &str) -> Result<Self, TitleError> {
        if text.len() > MAX_TITLE_BYTES {
            return Err(TitleError::TooLarge { len: text.len() });
        }
        if let Some(pos) = text.find('\r') {
            let line = text[..pos].chars().filter(|&c| c == '\n').count() + 1;
            return Err(TitleError::BadLineEnding { line });
        }

        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return Err(TitleError::MissingHeader { line: 1 });
        }

        let header = lines[0].trim();
        if !header.starts_with("# TITLE") {
            return Err(TitleError::MissingHeader { line: 1 });
        }

        let mut entries = Vec::new();
        for (idx, line) in lines.iter().enumerate().skip(1) {
            let line_num = idx + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let entry = Self::parse_line(trimmed, line_num)?;
            entries.push(entry);
        }

        Ok(Self {
            header: header.to_string(),
            entries,
        })
    }

    fn parse_line(line: &str, line_num: usize) -> Result<TitleEntry, TitleError> {
        if !line.starts_with("- [") {
            return Err(TitleError::ParseError {
                line: line_num,
                detail: format!("entry must start with '- [timestamp]', got '{line}'"),
            });
        }

        let end_bracket = line.find(']').ok_or_else(|| TitleError::ParseError {
            line: line_num,
            detail: "missing closing bracket for timestamp".to_string(),
        })?;
        let timestamp = line[3..end_bracket].trim().to_string();
        let rest = line[(end_bracket + 1)..].trim();

        let mut parts = rest.split_whitespace();
        let kind_token = parts.next().ok_or_else(|| TitleError::ParseError {
            line: line_num,
            detail: "missing entry kind token".to_string(),
        })?;

        match kind_token {
            "trigger" => {
                let mut commission = None;
                let mut slug = None;
                let payload = rest.trim_start_matches("trigger").trim();

                for token in payload.split_whitespace() {
                    if let Some(c) = token.strip_prefix("commission=") {
                        commission = c.parse().ok();
                    } else if let Some(s) = token.strip_prefix("slug=") {
                        slug = sanitize_name(s).ok();
                    }
                }
                let prompt = extract_quoted_param(payload, "prompt").unwrap_or_default();
                Ok(TitleEntry::Trigger {
                    timestamp,
                    commission,
                    slug,
                    prompt,
                })
            }
            "provision" => {
                let mut uid = None;
                let mut archetype = None;
                let mut blueprint = None;
                let mut version = None;

                for token in parts {
                    if let Some(u) = token.strip_prefix("uid=") {
                        uid = u.parse().ok();
                    } else if let Some(a) = token.strip_prefix("archetype=") {
                        archetype = sanitize_name(a).ok();
                    } else if let Some(b) = token.strip_prefix("blueprint=") {
                        blueprint = sanitize_name(b).ok();
                    } else if let Some(v) = token.strip_prefix("version=") {
                        version = v.parse().ok();
                    }
                }

                let uid = uid.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "provision missing 'uid='".to_string(),
                })?;
                let archetype = archetype.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "provision missing 'archetype='".to_string(),
                })?;
                let blueprint = blueprint.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "provision missing 'blueprint='".to_string(),
                })?;
                let version = version.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "provision missing 'version='".to_string(),
                })?;

                Ok(TitleEntry::Provision {
                    timestamp,
                    uid,
                    archetype,
                    blueprint,
                    version,
                })
            }
            "session" => {
                let mut session_id = None;
                let mut incarnation = None;

                for token in parts {
                    if let Some(s) = token.strip_prefix("session_id=") {
                        session_id = Some(s.to_string());
                    } else if let Some(i) = token.strip_prefix("incarnation=") {
                        incarnation = i.parse().ok();
                    }
                }

                let session_id = session_id.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "session missing 'session_id='".to_string(),
                })?;
                let incarnation = incarnation.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "session missing 'incarnation='".to_string(),
                })?;

                Ok(TitleEntry::SessionIncarnation {
                    timestamp,
                    session_id,
                    incarnation,
                })
            }
            "crash" => {
                let mut session_id = None;
                let mut exit_code = None;
                let mut restarts = None;
                let payload = rest.trim_start_matches("crash").trim();

                for token in payload.split_whitespace() {
                    if let Some(s) = token.strip_prefix("session_id=") {
                        session_id = Some(s.to_string());
                    } else if let Some(e) = token.strip_prefix("exit_code=") {
                        if e != "none" {
                            exit_code = e.parse().ok();
                        }
                    } else if let Some(r) = token.strip_prefix("restarts=") {
                        restarts = r.parse().ok();
                    }
                }

                let session_id = session_id.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "crash missing 'session_id='".to_string(),
                })?;
                let restarts = restarts.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "crash missing 'restarts='".to_string(),
                })?;
                let reason = extract_quoted_param(payload, "reason").unwrap_or_default();

                Ok(TitleEntry::Crash {
                    timestamp,
                    session_id,
                    exit_code,
                    restarts,
                    reason,
                })
            }
            "steer" => {
                let payload = rest.trim_start_matches("steer").trim();
                let directive = extract_quoted_param(payload, "directive")
                    .unwrap_or_else(|| payload.to_string());
                Ok(TitleEntry::Steer {
                    timestamp,
                    directive,
                })
            }
            "promotion" => {
                let mut kind = None;
                let mut target = None;
                let mut status = None;

                for token in parts {
                    if let Some(k) = token.strip_prefix("kind=") {
                        kind = Some(k.to_string());
                    } else if let Some(t) = token.strip_prefix("target=") {
                        target = Some(t.to_string());
                    } else if let Some(s) = token.strip_prefix("status=") {
                        status = Some(s.to_string());
                    }
                }

                let kind = kind.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "promotion missing 'kind='".to_string(),
                })?;
                let target = target.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "promotion missing 'target='".to_string(),
                })?;
                let status = status.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "promotion missing 'status='".to_string(),
                })?;

                Ok(TitleEntry::Promotion {
                    timestamp,
                    kind,
                    target,
                    status,
                })
            }
            "hibernate" => {
                let mut patron = None;
                let payload = rest.trim_start_matches("hibernate").trim();

                for token in payload.split_whitespace() {
                    if let Some(p) = token.strip_prefix("patron=") {
                        patron = sanitize_name(p).ok();
                    }
                }

                let patron = patron.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "hibernate missing 'patron='".to_string(),
                })?;
                let reason = extract_quoted_param(payload, "reason").unwrap_or_default();

                Ok(TitleEntry::Hibernate {
                    timestamp,
                    patron,
                    reason,
                })
            }
            "awaken" => {
                let mut patron = None;
                let mut from_commission = None;

                for token in parts {
                    if let Some(p) = token.strip_prefix("patron=") {
                        patron = sanitize_name(p).ok();
                    } else if let Some(fc) = token.strip_prefix("from_commission=") {
                        from_commission = fc.parse().ok();
                    }
                }

                let patron = patron.ok_or_else(|| TitleError::ParseError {
                    line: line_num,
                    detail: "awaken missing 'patron='".to_string(),
                })?;

                Ok(TitleEntry::Awaken {
                    timestamp,
                    patron,
                    from_commission,
                })
            }
            other => Err(TitleError::ParseError {
                line: line_num,
                detail: format!("unknown entry kind '{other}'"),
            }),
        }
    }

    /// Render the entire TITLE.md document.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str(&self.header);
        out.push_str("\n\n");
        for entry in &self.entries {
            out.push_str(&entry.render_line());
        }
        out
    }

    /// Verify append-only integrity (ADR-0039 cl. 4): asserts that `self.entries`
    /// contains `prev_entries` as an exact prefix in the same order.
    ///
    /// # Errors
    ///
    /// [`TitleError::NonAppendMutation`] if any previously recorded entry was modified or deleted.
    pub fn verify_append_only(&self, prev_entries: &[TitleEntry]) -> Result<(), TitleError> {
        if self.entries.len() < prev_entries.len() {
            return Err(TitleError::NonAppendMutation {
                detail: format!(
                    "entry count decreased from {} to {}",
                    prev_entries.len(),
                    self.entries.len()
                ),
            });
        }
        for (i, prev) in prev_entries.iter().enumerate() {
            if self.entries[i] != *prev {
                return Err(TitleError::NonAppendMutation {
                    detail: format!("historical entry {i} was modified or reordered"),
                });
            }
        }
        Ok(())
    }

    /// Compute FNV-1a content digest over the canonical rendered text for tamper detection.
    #[must_use]
    pub fn content_digest(&self) -> String {
        content_digest(self.render().as_bytes())
    }
}

fn extract_quoted_param(text: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start_idx = text.find(&needle)? + needle.len();
    let remainder = &text[start_idx..];

    let mut out = String::new();
    let mut chars = remainder.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next_c) = chars.next() {
                out.push(next_c);
            }
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Part 4: Dual Convergence Types
// ---------------------------------------------------------------------------

/// The promotion branch at commission convergence (SPEC §10, ADR-0039 cl. 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    /// Environment layer: DEED `### Dynamic Alterations` → blueprint evolution.
    DeedToBlueprint,
    /// Intelligence layer: PROPRIETOR mutations → hibernated patron record.
    ProprietorToPatron,
}

/// The result of an individual promotion branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum BranchOutcome {
    /// Promotion executed successfully.
    Succeeded {
        /// Explanation or target version.
        detail: String,
    },
    /// Promotion was cleanly skipped (e.g. no alterations to promote, or already merged).
    Skipped {
        /// Reason for skip.
        reason: String,
    },
    /// Promotion failed.
    Failed {
        /// Error explanation.
        error: String,
    },
}

impl BranchOutcome {
    /// True if outcome is Succeeded.
    #[must_use]
    pub fn is_succeeded(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }

    /// True if outcome is Skipped.
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    /// True if outcome is Failed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Combined convergence report for a single parcel's dual promotions (ADR-0039 cl. 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DualConvergenceReport {
    /// Parcel slot UID.
    pub parcel_uid: u32,
    /// Patron name.
    pub patron_name: SafeName,
    /// Outcome of DEED → blueprint promotion.
    pub deed_outcome: BranchOutcome,
    /// Outcome of PROPRIETOR → patron promotion.
    pub proprietor_outcome: BranchOutcome,
}

impl DualConvergenceReport {
    /// True if both branches either succeeded or were cleanly skipped.
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self.deed_outcome.is_failed() && !self.proprietor_outcome.is_failed()
    }

    /// True if either branch failed.
    #[must_use]
    pub fn has_failure(&self) -> bool {
        self.deed_outcome.is_failed() || self.proprietor_outcome.is_failed()
    }

    /// True if human escalation is required.
    #[must_use]
    pub fn requires_escalation(&self) -> bool {
        self.has_failure()
    }
}

/// Convergence summary across all parcels in a closed commission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionConvergenceSummary {
    /// Commission identifier.
    pub commission_id: u64,
    /// Reports per parcel UID.
    pub reports: BTreeMap<u32, DualConvergenceReport>,
}

impl CommissionConvergenceSummary {
    /// Build a summary for a commission.
    #[must_use]
    pub fn new(commission_id: u64) -> Self {
        Self {
            commission_id,
            reports: BTreeMap::new(),
        }
    }

    /// Record a parcel's convergence report.
    pub fn record(&mut self, report: DualConvergenceReport) {
        self.reports.insert(report.parcel_uid, report);
    }

    /// True if all parcels converged without failure.
    #[must_use]
    pub fn overall_success(&self) -> bool {
        self.reports.values().all(DualConvergenceReport::is_success)
    }

    /// List of parcel UIDs that experienced at least one failure.
    #[must_use]
    pub fn failed_parcels(&self) -> Vec<u32> {
        self.reports
            .values()
            .filter(|r| r.has_failure())
            .map(|r| r.parcel_uid)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Part 5: Resource Policy & Pressure-Triggered Hibernation
// ---------------------------------------------------------------------------

/// Input telemetry and metadata for evaluating an agent parcel for hibernation (ADR-0040 cl. 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateInput {
    /// Parcel slot UID.
    pub uid: u32,
    /// Agent identity.
    pub identity: SafeName,
    /// Session status string (e.g. `idle`, `busy`, `crashed`, `stopped`).
    pub session_status: String,
    /// Whether the agent's commission is currently active (vs winding down or closed).
    pub is_active_in_commission: bool,
    /// Seconds since last task or stream activity.
    pub idle_duration_secs: u64,
    /// Sampled container memory usage in bytes.
    pub memory_usage_bytes: u64,
    /// Configured container memory limit in bytes.
    pub memory_limit_bytes: u64,
    /// Consecutive crash count.
    pub consecutive_crashes: u32,
    /// Whether the session has unread/pending mail or task directives.
    pub has_unprocessed_tasks: bool,
}

/// A scored candidate for hibernation surfaced by the daemon (ADR-0040 cl. 3–4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HibernateCandidate {
    /// Parcel slot UID.
    pub uid: u32,
    /// Agent identity.
    pub identity: SafeName,
    /// Ranking score (higher score = stronger candidate).
    pub score: u64,
    /// Idle duration in seconds.
    pub idle_duration_secs: u64,
    /// Sampled memory usage in bytes.
    pub memory_usage_bytes: u64,
    /// Configured memory limit in bytes.
    pub memory_limit_bytes: u64,
    /// Structured evidence explaining why this candidate was selected.
    pub evidence: String,
}

/// Pure ranking algorithm for candidate selection under resource pressure (ADR-0040 cl. 4).
///
/// Deterministic scoring:
/// - Status weight: `Crashed` (10,000) > `Stopped` (8,000) > `Idle` (5,000) > `Busy` (0)
/// - Penalize busy sessions or sessions with unprocessed tasks
/// - Prioritize sessions on non-active commissions (WindingDown / Closed: +2,000)
/// - Proportional points for idle duration (+ up to 3,600 points)
/// - Proportional points for memory footprint (+10 points per MiB reclaimed)
/// - Points for repeated crashes (+500 per crash, up to 2,500)
///
/// Sorted descending by score; ties broken by ascending UID.
#[must_use]
pub fn rank_candidates(
    inputs: &[CandidateInput],
    memory_pressure_threshold_pct: u8,
) -> Vec<HibernateCandidate> {
    let mut candidates = Vec::with_capacity(inputs.len());

    for input in inputs {
        let mut score: u64 = 0;

        let status_lower = input.session_status.to_ascii_lowercase();
        match status_lower.as_str() {
            "crashed" => score += 10_000,
            "stopped" => score += 8_000,
            "idle" => score += 5_000,
            "busy" => {
                // Busy agents are protected unless critically OOMing
                score += 0;
            }
            _ => score += 1_000,
        }

        if input.has_unprocessed_tasks && score >= 3_000 {
            score -= 3_000;
        }

        if !input.is_active_in_commission {
            score += 2_000;
        }

        let idle_capped = input.idle_duration_secs.min(3_600);
        score += idle_capped;

        let mib_used = input.memory_usage_bytes / (1024 * 1024);
        score += mib_used.saturating_mul(10);

        let crash_pts = (input.consecutive_crashes as u64)
            .min(5)
            .saturating_mul(500);
        score += crash_pts;

        let usage_pct = if input.memory_limit_bytes > 0 {
            (input.memory_usage_bytes as f64 / input.memory_limit_bytes as f64 * 100.0) as u64
        } else {
            0
        };

        let evidence = format!(
            "agent {} (UID {}): status={}, idle={}s, mem={} MiB ({}% of limit {} MiB), crashes={}, tasks_pending={}; pressure_threshold={}%",
            input.identity,
            input.uid,
            input.session_status,
            input.idle_duration_secs,
            mib_used,
            usage_pct,
            input.memory_limit_bytes / (1024 * 1024),
            input.consecutive_crashes,
            input.has_unprocessed_tasks,
            memory_pressure_threshold_pct,
        );

        candidates.push(HibernateCandidate {
            uid: input.uid,
            identity: input.identity.clone(),
            score,
            idle_duration_secs: input.idle_duration_secs,
            memory_usage_bytes: input.memory_usage_bytes,
            memory_limit_bytes: input.memory_limit_bytes,
            evidence,
        });
    }

    // Sort descending by score; ties broken by ascending UID.
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.uid.cmp(&b.uid)));
    candidates
}

/// Action recommendation for container OOM crashes under Sprint 03 restart policy (ADR-0040 cl. 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OomAction {
    /// Restart the container with exponential backoff.
    RestartWithBackoff {
        /// Attempt number.
        attempt: u32,
        /// Backoff duration in milliseconds.
        backoff_ms: u64,
    },
    /// Restart budget exhausted: session remains `Crashed`, Mayor is woken.
    BudgetExhaustedWakeMayor {
        /// Total consecutive crashes.
        total_crashes: u32,
        /// Evidence explanation.
        evidence: String,
    },
    /// Repeated OOMs under high memory pressure suggest early hibernation.
    SuggestEarlyHibernation {
        /// Number of OOM crashes observed.
        oom_count: u32,
        /// Memory usage measured.
        memory_bytes: u64,
        /// Evidence explanation.
        evidence: String,
    },
}

/// Pure evaluation of OOM crash policy against restart limits (ADR-0040 cl. 5).
#[must_use]
pub fn evaluate_oom_policy(
    consecutive_crashes: u32,
    max_restarts: u32,
    backoff_initial_ms: u64,
    backoff_max_ms: u64,
    memory_usage_bytes: u64,
    memory_limit_bytes: u64,
) -> OomAction {
    if consecutive_crashes >= max_restarts {
        return OomAction::BudgetExhaustedWakeMayor {
            total_crashes: consecutive_crashes,
            evidence: format!(
                "OOM restart budget exhausted ({consecutive_crashes}/{max_restarts} restarts); mem={}/{} bytes",
                memory_usage_bytes, memory_limit_bytes
            ),
        };
    }

    // Repeated OOMs (>= 2) under critical memory usage (>90% of limit) suggest early hibernation
    let usage_pct = if memory_limit_bytes > 0 {
        (memory_usage_bytes as f64 / memory_limit_bytes as f64 * 100.0) as u64
    } else {
        0
    };

    if consecutive_crashes >= 2 && usage_pct >= 90 {
        return OomAction::SuggestEarlyHibernation {
            oom_count: consecutive_crashes,
            memory_bytes: memory_usage_bytes,
            evidence: format!(
                "repeated OOM kills ({consecutive_crashes}) under critical memory ({usage_pct}% of limit); suggest stasis"
            ),
        };
    }

    // Standard exponential backoff calculation (Sprint 03)
    let shift = (consecutive_crashes.saturating_sub(1)).min(63);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let backoff_ms = backoff_initial_ms
        .saturating_mul(factor)
        .min(backoff_max_ms);

    OomAction::RestartWithBackoff {
        attempt: consecutive_crashes + 1,
        backoff_ms,
    }
}

// ---------------------------------------------------------------------------
// Unit tests & Golden Vectors
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> SafeName {
        sanitize_name(s).unwrap()
    }

    #[test]
    fn patron_doc_round_trip_golden() {
        let input = "\
---
patron: scribe
register: scribe
blueprint: default
blueprint_version: 1
source_uid: 1001
commission: 1
tags: [\"cli\", \"rust\"]
created_at: 1729300000
stasis_version: 1
---

# Intelligence Profile — Scribe

Specialized in CLI tool development and Rust architectural documentation.
";
        let doc = PatronDoc::parse(input).expect("patron doc parses cleanly");
        assert_eq!(doc.frontmatter.patron, name("scribe"));
        assert_eq!(doc.frontmatter.register, name("scribe"));
        assert_eq!(doc.frontmatter.blueprint, name("default"));
        assert_eq!(doc.frontmatter.blueprint_version, 1);
        assert_eq!(doc.frontmatter.source_uid, 1001);
        assert_eq!(doc.frontmatter.commission, Some(1));
        assert!(doc.frontmatter.tags.contains(&name("rust")));
        assert!(doc.frontmatter.tags.contains(&name("cli")));
        assert_eq!(doc.frontmatter.created_at, 1729300000);

        let rendered = doc.render();
        let doc2 = PatronDoc::parse(&rendered).expect("rendered patron doc round-trips");
        assert_eq!(doc, doc2);

        // Matching rule
        assert!(doc.matches_skill_config(&name("scribe"), &name("default"), &[name("rust")]));
        assert!(!doc.matches_skill_config(&name("worker"), &name("default"), &[name("rust")]));
        assert!(!doc.matches_skill_config(&name("scribe"), &name("default"), &[name("python")]));
    }

    #[test]
    fn patron_doc_rejections() {
        // CRLF rejection
        let crlf = "---\r\npatron: scribe\r\n---\nbody\n";
        assert!(matches!(
            PatronDoc::parse(crlf),
            Err(PatronError::BadLineEnding { .. })
        ));

        // Missing opening fence
        let no_start = "patron: scribe\n---\nbody\n";
        assert!(matches!(
            PatronDoc::parse(no_start),
            Err(PatronError::MissingFrontmatterStart)
        ));

        // Missing closing fence
        let no_end = "---\npatron: scribe\nbody\n";
        assert!(matches!(
            PatronDoc::parse(no_end),
            Err(PatronError::MissingFrontmatterEnd)
        ));

        // Invalid UID
        let bad_uid = "---\npatron: scribe\nregister: scribe\nblueprint: default\nblueprint_version: 1\nsource_uid: 999\ncreated_at: 1000\n---\nbody\n";
        assert!(matches!(
            PatronDoc::parse(bad_uid),
            Err(PatronError::InvalidField {
                field: "source_uid",
                ..
            })
        ));
    }

    #[test]
    fn backup_manifest_round_trip_golden() {
        let mut entries = BTreeMap::new();
        entries.insert(
            ".gitconfig".to_string(),
            ManifestEntry {
                entry_type: ManifestEntryType::File,
                digest: "af63dc4c8601ec8c".to_string(),
                size_bytes: 128,
                mode: 0o600,
            },
        );
        entries.insert(
            ".hamlet".to_string(),
            ManifestEntry {
                entry_type: ManifestEntryType::Directory,
                digest: "85944171f73967e8".to_string(),
                size_bytes: 4096,
                mode: 0o755,
            },
        );

        let manifest = BackupManifest {
            manifest_version: 1,
            patron: name("scribe"),
            source_uid: 1001,
            created_at: 1729300000,
            total_bytes: 4224,
            entries,
        };

        let rendered = manifest.render().expect("manifest renders to JSON");
        assert!(rendered.contains("\"manifest_version\": 1"));
        assert!(rendered.contains("\"patron\": \"scribe\""));

        let parsed = BackupManifest::parse(&rendered).expect("manifest parses back");
        assert_eq!(manifest, parsed);

        // Verification checks
        assert!(manifest
            .verify_entry(
                ".gitconfig",
                ManifestEntryType::File,
                128,
                "af63dc4c8601ec8c"
            )
            .is_ok());

        // Digest mismatch
        assert!(manifest
            .verify_entry(
                ".gitconfig",
                ManifestEntryType::File,
                128,
                "0000000000000000"
            )
            .is_err());

        // Size mismatch
        assert!(manifest
            .verify_entry(
                ".gitconfig",
                ManifestEntryType::File,
                256,
                "af63dc4c8601ec8c"
            )
            .is_err());

        // Missing entry
        assert!(manifest
            .verify_entry(".profile", ManifestEntryType::File, 64, "af63dc4c8601ec8c")
            .is_err());
    }

    #[test]
    fn title_doc_round_trip_all_eight_entry_types_golden() {
        let input = "\
# TITLE — parcel1001

- [2026-10-18T23:50:00Z] trigger commission=1 slug=watchtower prompt=\"Build the watchtower module\"
- [2026-10-18T23:50:05Z] provision uid=1001 archetype=scribe blueprint=default version=1
- [2026-10-18T23:50:10Z] session session_id=s-1001-0001 incarnation=1
- [2026-10-18T23:50:20Z] steer directive=\"Focus on error handling\"
- [2026-10-18T23:50:35Z] crash session_id=s-1001-0001 exit_code=137 restarts=1 reason=\"OOMKilled by kernel\"
- [2026-10-18T23:50:50Z] promotion kind=blueprint target=default.v2 status=promoted
- [2026-10-18T23:51:10Z] hibernate patron=scribe reason=\"task completed cleanly\"
- [2026-10-18T23:51:40Z] awaken patron=scribe from_commission=1
";
        let doc = TitleDoc::parse(input).expect("title doc parses cleanly");
        assert_eq!(doc.entries.len(), 8);
        assert_eq!(doc.entries[0].kind(), TitleEntryKind::Trigger);
        assert_eq!(doc.entries[1].kind(), TitleEntryKind::Provision);
        assert_eq!(doc.entries[2].kind(), TitleEntryKind::SessionIncarnation);
        assert_eq!(doc.entries[3].kind(), TitleEntryKind::Steer);
        assert_eq!(doc.entries[4].kind(), TitleEntryKind::Crash);
        assert_eq!(doc.entries[5].kind(), TitleEntryKind::Promotion);
        assert_eq!(doc.entries[6].kind(), TitleEntryKind::Hibernate);
        assert_eq!(doc.entries[7].kind(), TitleEntryKind::Awaken);

        let rendered = doc.render();
        let doc2 = TitleDoc::parse(&rendered).expect("rendered title doc parses back");
        assert_eq!(doc, doc2);

        // Append-only check passes on identical or extended
        assert!(doc2.verify_append_only(&doc.entries).is_ok());

        // Append an entry
        let mut extended = doc.clone();
        extended.entries.push(TitleEntry::Steer {
            timestamp: "2026-10-18T23:52:00Z".to_string(),
            directive: "Carry on".to_string(),
        });
        assert!(extended.verify_append_only(&doc.entries).is_ok());

        // Deleting or modifying a historical entry fails
        let mut tampered = doc.clone();
        tampered.entries.remove(0);
        assert!(tampered.verify_append_only(&doc.entries).is_err());
    }

    #[test]
    fn title_doc_rejections() {
        // Missing header
        assert!(matches!(
            TitleDoc::parse("- [ts] session session_id=s-1 incarnation=1\n"),
            Err(TitleError::MissingHeader { .. })
        ));

        // Bad line ending
        assert!(matches!(
            TitleDoc::parse("# TITLE — parcel1001\r\n- [ts] steer directive=\"x\"\n"),
            Err(TitleError::BadLineEnding { .. })
        ));

        // Malformed line
        assert!(matches!(
            TitleDoc::parse("# TITLE — parcel1001\nnot a valid entry bullet\n"),
            Err(TitleError::ParseError { .. })
        ));
    }

    #[test]
    fn dual_convergence_tracking() {
        let ok_report = DualConvergenceReport {
            parcel_uid: 1001,
            patron_name: name("scribe"),
            deed_outcome: BranchOutcome::Succeeded {
                detail: "promoted to v2".to_string(),
            },
            proprietor_outcome: BranchOutcome::Succeeded {
                detail: "patron recorded".to_string(),
            },
        };
        assert!(ok_report.is_success());
        assert!(!ok_report.has_failure());

        let mixed_report = DualConvergenceReport {
            parcel_uid: 1002,
            patron_name: name("worker"),
            deed_outcome: BranchOutcome::Failed {
                error: "syntax error in DEED.md".to_string(),
            },
            proprietor_outcome: BranchOutcome::Succeeded {
                detail: "patron recorded".to_string(),
            },
        };
        assert!(!mixed_report.is_success());
        assert!(mixed_report.has_failure());
        assert!(mixed_report.requires_escalation());

        let mut summary = CommissionConvergenceSummary::new(1);
        summary.record(ok_report);
        assert!(summary.overall_success());

        summary.record(mixed_report);
        assert!(!summary.overall_success());
        assert_eq!(summary.failed_parcels(), vec![1002]);
    }

    #[test]
    fn candidate_selection_ranking_truth_table() {
        let inputs = vec![
            CandidateInput {
                uid: 1001,
                identity: name("busy-worker"),
                session_status: "busy".to_string(),
                is_active_in_commission: true,
                idle_duration_secs: 10,
                memory_usage_bytes: 400 * 1024 * 1024,
                memory_limit_bytes: 512 * 1024 * 1024,
                consecutive_crashes: 0,
                has_unprocessed_tasks: true,
            },
            CandidateInput {
                uid: 1002,
                identity: name("idle-worker"),
                session_status: "idle".to_string(),
                is_active_in_commission: false,
                idle_duration_secs: 1800,
                memory_usage_bytes: 450 * 1024 * 1024,
                memory_limit_bytes: 512 * 1024 * 1024,
                consecutive_crashes: 0,
                has_unprocessed_tasks: false,
            },
            CandidateInput {
                uid: 1003,
                identity: name("crashed-worker"),
                session_status: "crashed".to_string(),
                is_active_in_commission: false,
                idle_duration_secs: 300,
                memory_usage_bytes: 200 * 1024 * 1024,
                memory_limit_bytes: 512 * 1024 * 1024,
                consecutive_crashes: 3,
                has_unprocessed_tasks: false,
            },
        ];

        let ranked = rank_candidates(&inputs, 85);
        assert_eq!(ranked.len(), 3);
        // Crashed or idle non-active workers rank above busy workers
        assert_eq!(ranked[0].uid, 1003); // crashed status carries 10,000 base weight
        assert_eq!(ranked[1].uid, 1002); // idle on inactive commission
        assert_eq!(ranked[2].uid, 1001); // busy worker protected at bottom
        assert!(ranked[0].score > ranked[1].score);
        assert!(ranked[1].score > ranked[2].score);
    }

    #[test]
    fn oom_policy_evaluation_truth_table() {
        // Attempt 1: backoff initial
        let action = evaluate_oom_policy(0, 5, 1000, 60000, 200 * 1024 * 1024, 512 * 1024 * 1024);
        assert_eq!(
            action,
            OomAction::RestartWithBackoff {
                attempt: 1,
                backoff_ms: 1000
            }
        );

        // Attempt 2: exponential backoff
        let action = evaluate_oom_policy(1, 5, 1000, 60000, 200 * 1024 * 1024, 512 * 1024 * 1024);
        assert_eq!(
            action,
            OomAction::RestartWithBackoff {
                attempt: 2,
                backoff_ms: 1000
            }
        );

        // Consecutive 2 OOMs under critical memory usage (>90%): suggest early stasis
        let action = evaluate_oom_policy(2, 5, 1000, 60000, 480 * 1024 * 1024, 512 * 1024 * 1024);
        assert!(matches!(action, OomAction::SuggestEarlyHibernation { .. }));

        // Budget exhausted (>= 5): wake Mayor
        let action = evaluate_oom_policy(5, 5, 1000, 60000, 400 * 1024 * 1024, 512 * 1024 * 1024);
        assert!(matches!(action, OomAction::BudgetExhaustedWakeMayor { .. }));
    }
}
