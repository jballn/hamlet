//! DEED/vault promotion parsing (SPEC §9.4, §10 dual-state convergence).
//!
//! Parses DEED.md `### Dynamic Alterations` blocks (environment layer →
//! blueprint write-back) and names vault promotion requests out of
//! `/var/freight/` into `keep/vault/`. Parsing/validation only — promotion
//! copies are daemon-performed, with every name through [`crate::sanitize`]
//! (ADR-0038: the purity seam holds in core; step 06 executes).
//!
//! # Grammar (ADR-0038 cl. 1)
//!
//! At most one section headed exactly `### Dynamic Alterations`; zero is a
//! legal no-op, two is an error. Inside the section only blank lines and
//! alteration bullets are legal:
//!
//! ```text
//! - <kind> <name>[ = <version_spec>]
//! ```
//!
//! with `kind ∈ {pixi, nvm, cargo, apk}` and name/spec reusing the
//! blueprint's own validators (one vocabulary for "what counts as a tool").
//! Errors are line-numbered throughout; LF-only, as at every text boundary.

use std::collections::BTreeSet;
use std::fmt;

use crate::blueprint::{
    tool_name_re, version_spec_re, Blueprint, BlueprintError, ToolKind, ToolSpec,
    MAX_TOOL_NAME_LEN, MAX_VERSION_SPEC_LEN,
};
use crate::decree::{TreeRelPath, FREIGHT_PREFIX};
use crate::sanitize::{sanitize_name, SafeName, SanitizeError};

// ---------------------------------------------------------------------------
// Alterations (DEED grammar)
// ---------------------------------------------------------------------------

/// One alteration line from a DEED `### Dynamic Alterations` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alteration {
    kind: ToolKind,
    name: String,
    version_spec: Option<String>,
}

impl Alteration {
    /// Provisioning channel.
    #[must_use]
    pub fn kind(&self) -> ToolKind {
        self.kind
    }

    /// Validated tool name (blueprint charset, ADR-0009).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Version spec; `None` = channel default.
    #[must_use]
    pub fn version_spec(&self) -> Option<&str> {
        self.version_spec.as_deref()
    }
}

/// DEED `### Dynamic Alterations` parse failure (ADR-0038 cl. 1). Every
/// variant names the offending line (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeedError {
    /// A second section heading — at most one is legal.
    DuplicateSection {
        /// 1-based line of the second heading.
        line: usize,
    },
    /// Inside the section, a line that is neither blank nor an alteration
    /// bullet.
    IllegalLine {
        /// 1-based line.
        line: usize,
        /// The offending text (trimmed).
        text: String,
    },
    /// Kind outside pixi|nvm|cargo|apk.
    UnknownKind {
        /// 1-based line.
        line: usize,
        /// The offending token.
        value: String,
    },
    /// Name fails the blueprint tool-name validator.
    InvalidName {
        /// 1-based line.
        line: usize,
        /// The offending name.
        value: String,
    },
    /// Version spec fails the blueprint validator.
    InvalidVersionSpec {
        /// 1-based line.
        line: usize,
        /// The offending spec.
        value: String,
    },
    /// Same (kind, name) twice in one section.
    DuplicateTool {
        /// 1-based line of the second occurrence.
        line: usize,
        /// Channel.
        kind: ToolKind,
        /// The tool name.
        name: String,
    },
    /// CR detected — LF-only at every text boundary (ADR-0019 cl. 1).
    NonLfLineEnding {
        /// 1-based line.
        line: usize,
    },
    /// Non-append mutation: baseline was deleted or previously recorded
    /// alterations were modified or deleted (SPEC §9.4).
    NonAppendMutation {
        /// Detail of the violation.
        detail: String,
    },
}

impl fmt::Display for DeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSection { line } => {
                write!(
                    f,
                    "line {line}: a second '### Dynamic Alterations' section (at most one)"
                )
            }
            Self::IllegalLine { line, text } => {
                write!(f, "line {line}: not an alteration bullet: {text:?}")
            }
            Self::UnknownKind { line, value } => write!(
                f,
                "line {line}: unknown tool kind {value:?} (expected pixi|nvm|cargo|apk)"
            ),
            Self::InvalidName { line, value } => {
                write!(f, "line {line}: invalid tool name {value:?}")
            }
            Self::InvalidVersionSpec { line, value } => {
                write!(f, "line {line}: invalid version spec {value:?}")
            }
            Self::DuplicateTool { line, kind, name } => {
                write!(f, "line {line}: duplicate tool {kind} {name:?}")
            }
            Self::NonLfLineEnding { line } => {
                write!(
                    f,
                    "line {line}: CR detected; DEED text is LF-only (ADR-0019 cl. 1)"
                )
            }
            Self::NonAppendMutation { detail } => {
                write!(f, "non-append DEED mutation: {detail}")
            }
        }
    }
}

impl std::error::Error for DeedError {}

/// Parse a DEED.md's `### Dynamic Alterations` section (ADR-0038 cl. 1).
/// Zero sections → empty vec (legal no-op); two → [`DeedError::DuplicateSection`].
/// Pure: reads only its input bytes.
///
/// # Errors
///
/// [`DeedError`] naming the first violation, line-numbered.
pub fn parse_deed_alterations(deed: &str) -> Result<Vec<Alteration>, DeedError> {
    const SECTION_HEADING: &str = "### Dynamic Alterations";
    let mut in_section = false;
    let mut in_baseline = false;
    let mut out: Vec<Alteration> = Vec::new();
    let mut seen: BTreeSet<(ToolKind, String)> = BTreeSet::new();

    for (index, line) in deed.split('\n').enumerate() {
        let line_no = index + 1;
        if line.contains('\r') {
            return Err(DeedError::NonLfLineEnding { line: line_no });
        }
        let trimmed = line.trim();

        if in_section {
            if trimmed == SECTION_HEADING {
                return Err(DeedError::DuplicateSection { line: line_no });
            }
            // Any other heading ends the section; its content is ignored.
            if trimmed.starts_with('#') {
                in_section = false;
                in_baseline = false;
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            // Baseline entry floor check (SPEC §9.4; bootstrap records toolchain baseline
            // inside the alterations block; this is the environment floor, not an alteration).
            if in_baseline {
                if line.starts_with("  ")
                    || line.starts_with('\t')
                    || trimmed.starts_with("- manifest:")
                    || trimmed.starts_with("- tool:")
                    || trimmed.starts_with("- image layer")
                    || trimmed.starts_with("- env keys:")
                {
                    continue;
                }
                in_baseline = false;
            }
            if trimmed.starts_with("- baseline · ") {
                in_baseline = true;
                continue;
            }
            let alteration = parse_line(trimmed, line_no)?;
            let key = (alteration.kind(), alteration.name().to_owned());
            if !seen.insert(key) {
                return Err(DeedError::DuplicateTool {
                    line: line_no,
                    kind: alteration.kind(),
                    name: alteration.name().to_owned(),
                });
            }
            out.push(alteration);
        } else if trimmed == SECTION_HEADING {
            in_section = true;
        }
    }
    Ok(out)
}

/// Parse the parent blueprint name from the DEED header (`- blueprint: <name> v<version>`).
pub fn parse_deed_blueprint_ref(deed: &str) -> Option<SafeName> {
    for line in deed.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- blueprint: ") {
            let name_part = rest.split_whitespace().next()?;
            return sanitize_name(name_part).ok();
        }
    }
    None
}

/// Check if an alteration set is already merged into a blueprint.
/// Returns true if every alteration in `alterations` is already present
/// in `blueprint.tools()` with matching kind, name, and version_spec.
pub fn is_already_merged(blueprint: &Blueprint, alterations: &[Alteration]) -> bool {
    if alterations.is_empty() {
        return true;
    }
    alterations.iter().all(|alt| {
        blueprint.tools().iter().any(|tool| {
            tool.kind() == alt.kind()
                && tool.name() == alt.name()
                && tool.version_spec() == alt.version_spec()
        })
    })
}

/// Deterministic digest of an alteration set (FNV-1a/64 hex).
#[must_use]
pub fn alterations_digest(alterations: &[Alteration]) -> String {
    let mut lines: Vec<String> = alterations
        .iter()
        .map(|alt| {
            if let Some(spec) = alt.version_spec() {
                format!("- {} {} = {}\n", alt.kind(), alt.name(), spec)
            } else {
                format!("- {} {}\n", alt.kind(), alt.name())
            }
        })
        .collect();
    lines.sort();
    let canonical = lines.concat();
    crate::decree::content_digest(canonical.as_bytes())
}

/// One section line: `- <kind> <name>[ = <version_spec>]`. Names and specs
/// cannot contain spaces (blueprint validators), so `" = "` is the only legal
/// separator between name and spec.
fn parse_line(trimmed: &str, line_no: usize) -> Result<Alteration, DeedError> {
    let illegal = || DeedError::IllegalLine {
        line: line_no,
        text: trimmed.to_owned(),
    };
    let Some(rest) = trimmed.strip_prefix("- ") else {
        return Err(illegal());
    };
    let Some((kind_str, remainder)) = rest.split_once(' ') else {
        return Err(illegal());
    };
    let kind = match kind_str {
        "pixi" => ToolKind::Pixi,
        "nvm" => ToolKind::Nvm,
        "cargo" => ToolKind::Cargo,
        "apk" => ToolKind::Apk,
        other => {
            return Err(DeedError::UnknownKind {
                line: line_no,
                value: other.to_owned(),
            })
        }
    };
    let (name, version_spec) = match remainder.rfind(" = ") {
        Some(idx) => (&remainder[..idx], Some(&remainder[idx + 3..])),
        None => (remainder, None),
    };
    if name.len() > MAX_TOOL_NAME_LEN || !tool_name_re().is_match(name) {
        return Err(DeedError::InvalidName {
            line: line_no,
            value: name.to_owned(),
        });
    }
    let version_spec = match version_spec {
        None => None,
        Some(spec) => {
            if spec.len() > MAX_VERSION_SPEC_LEN || !version_spec_re().is_match(spec) {
                return Err(DeedError::InvalidVersionSpec {
                    line: line_no,
                    value: spec.to_owned(),
                });
            }
            Some(spec.to_owned())
        }
    };
    Ok(Alteration {
        kind,
        name: name.to_owned(),
        version_spec,
    })
}

// ---------------------------------------------------------------------------
// Upsert (ADR-0038 cl. 2)
// ---------------------------------------------------------------------------

/// Apply parsed alterations to a blueprint (ADR-0038 cl. 2): an existing
/// `(kind, name)` gets its `version_spec` replaced; unknown tools append in
/// declaration order; `meta.version` increments by one (refused at
/// `u32::MAX`). The result is a *fresh* immutable [`Blueprint`] — promotion
/// is replacement, never mutation.
///
/// # Errors
///
/// [`BlueprintError::VersionOverflow`] when the version cannot be bumped;
/// otherwise infallible (the alterations were already validated at parse).
pub fn apply_alterations(
    blueprint: &Blueprint,
    alterations: &[Alteration],
) -> Result<Blueprint, BlueprintError> {
    let mut tools: Vec<_> = blueprint.tools().to_vec();
    for alteration in alterations {
        let idx = tools
            .iter()
            .position(|tool| tool.kind() == alteration.kind() && tool.name() == alteration.name());
        match idx {
            Some(i) => {
                let current = tools[i].clone();
                tools[i] =
                    current.with_version_spec(alteration.version_spec().map(str::to_owned))?;
            }
            None => {
                tools.push(ToolSpec::new(
                    alteration.kind(),
                    alteration.name().to_owned(),
                    alteration.version_spec().map(str::to_owned),
                )?);
            }
        }
    }
    let meta =
        blueprint
            .meta()
            .bumped_version()
            .ok_or_else(|| BlueprintError::VersionOverflow {
                path: "meta.version".into(),
            })?;
    Ok(Blueprint::assemble(
        meta,
        tools,
        blueprint.env().clone(),
        blueprint.skills().to_vec(),
    ))
}

// ---------------------------------------------------------------------------
// Vault promotion requests (ADR-0038 cl. 3)
// ---------------------------------------------------------------------------

/// A transient vault-promotion request: names a deliverable for promotion
/// out of `/var/freight/` into `keep/vault/`. Not charter state — no serde
/// persistence; the daemon validates it at convergence and executes the
/// `ApprovePromotion` decree (ADR-0026 cl. 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionRequest {
    commission: u64,
    freight_path: TreeRelPath,
    artifact_name: SafeName,
}

impl PromotionRequest {
    /// Validate and build a request. Rules mirror the decree side's freight
    /// rules so a request cannot out-run an `ApprovePromotion` decree: the
    /// path parses as a tree rel-path, sits under [`FREIGHT_PREFIX`], has ≥ 3
    /// components (a file, not the directory), and the artifact name
    /// sanitizes.
    ///
    /// # Errors
    ///
    /// [`PromotionError`] naming the first refusal.
    pub fn new(
        commission: u64,
        freight_path: &str,
        artifact_name: &str,
    ) -> Result<Self, PromotionError> {
        let path = TreeRelPath::parse(freight_path).map_err(PromotionError::FreightPath)?;
        if !path.starts_with(FREIGHT_PREFIX) || path.components().len() < 3 {
            return Err(PromotionError::NotAFreightFile {
                path: freight_path.to_owned(),
            });
        }
        let name =
            sanitize_name(artifact_name).map_err(|source| PromotionError::InvalidArtifactName {
                value: artifact_name.to_owned(),
                source,
            })?;
        Ok(Self {
            commission,
            freight_path: path,
            artifact_name: name,
        })
    }

    /// The commission this deliverable belongs to.
    #[must_use]
    pub fn commission(&self) -> u64 {
        self.commission
    }

    /// Validated village-relative freight source.
    #[must_use]
    pub fn freight_path(&self) -> &TreeRelPath {
        &self.freight_path
    }

    /// Sanitized artifact name for the vault.
    #[must_use]
    pub fn artifact_name(&self) -> &SafeName {
        &self.artifact_name
    }
}

/// Promotion request validation failure (ADR-0038 cl. 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionError {
    /// `freight_path` failed tree rel-path parsing (decree vocabulary).
    FreightPath(crate::decree::DecreeError),
    /// Path is not under `courier/freight/` or shallower than 3 components.
    NotAFreightFile {
        /// The offending path as given.
        path: String,
    },
    /// Artifact name failed the CONVENTIONS §4 sanitizer.
    InvalidArtifactName {
        /// The offending name as given.
        value: String,
        /// Sanitizer detail.
        source: SanitizeError,
    },
}

impl fmt::Display for PromotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FreightPath(source) => write!(f, "promotion freight path invalid: {source}"),
            Self::NotAFreightFile { path } => write!(
                f,
                "promotion source {path:?} is not a file under {FREIGHT_PREFIX}/ (≥ 3 components)"
            ),
            Self::InvalidArtifactName { value, source } => {
                write!(f, "artifact name invalid: {value:?}: {source}")
            }
        }
    }
}

impl std::error::Error for PromotionError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn blueprint_toml(version: u32, tools: &str) -> Blueprint {
        Blueprint::from_toml(&format!(
            r#"
[meta]
name = "village-alpine"
version = {version}
base_image = "alpine:3.20"
{tools}"#
        ))
        .unwrap()
    }

    #[test]
    fn no_section_is_a_legal_noop() {
        let deed = "# DEED\n\n### Convergence\n- nothing here\n";
        assert_eq!(
            parse_deed_alterations(deed).unwrap(),
            Vec::<Alteration>::new()
        );
        assert_eq!(
            parse_deed_alterations("").unwrap(),
            Vec::<Alteration>::new()
        );
    }

    #[test]
    fn section_parses_kinds_names_and_specs() {
        let deed = "# DEED\n\n### Dynamic Alterations\n- pixi uv = 0.4.2\n- nvm node\n- apk g++\n";
        let alterations = parse_deed_alterations(deed).unwrap();
        assert_eq!(alterations.len(), 3);
        assert_eq!(alterations[0].kind(), ToolKind::Pixi);
        assert_eq!(alterations[0].name(), "uv");
        assert_eq!(alterations[0].version_spec(), Some("0.4.2"));
        assert_eq!(alterations[1].kind(), ToolKind::Nvm);
        assert_eq!(alterations[1].version_spec(), None);
        assert_eq!(alterations[2].kind(), ToolKind::Apk);
        assert_eq!(alterations[2].name(), "g++");
    }

    #[test]
    fn blank_lines_between_entries_are_legal() {
        let deed = "### Dynamic Alterations\n\n- pixi uv\n\n- apk jq\n";
        assert_eq!(parse_deed_alterations(deed).unwrap().len(), 2);
    }

    #[test]
    fn a_second_section_is_an_error_with_line() {
        let deed = "### Dynamic Alterations\n- pixi uv\n\n### Dynamic Alterations\n- apk jq\n";
        let err = parse_deed_alterations(deed).unwrap_err();
        assert_eq!(err, DeedError::DuplicateSection { line: 4 });
    }

    #[test]
    fn a_following_heading_ends_the_section() {
        // Content after the next heading is ignored, not an error.
        let deed =
            "### Dynamic Alterations\n- pixi uv\n## Notes\n- this is not an alteration line\n";
        assert_eq!(parse_deed_alterations(deed).unwrap().len(), 1);
    }

    #[test]
    fn crlf_is_refused_with_line() {
        let deed = "### Dynamic Alterations\r\n- pixi uv\n";
        let err = parse_deed_alterations(deed).unwrap_err();
        assert_eq!(err, DeedError::NonLfLineEnding { line: 1 });
    }

    #[test]
    fn unknown_kind_is_line_numbered() {
        let deed = "### Dynamic Alterations\n- npm uv\n";
        let err = parse_deed_alterations(deed).unwrap_err();
        assert_eq!(
            err,
            DeedError::UnknownKind {
                line: 2,
                value: "npm".into()
            }
        );
    }

    #[test]
    fn invalid_name_and_spec_are_line_numbered() {
        let err =
            parse_deed_alterations("### Dynamic Alterations\n- pixi uv;rm -rf /\n").unwrap_err();
        assert_eq!(
            err,
            DeedError::InvalidName {
                line: 2,
                value: "uv;rm -rf /".into()
            }
        );

        let err = parse_deed_alterations("### Dynamic Alterations\n- pixi uv = not a version \n")
            .unwrap_err();
        // `not a version ` (trailing space) fails the spec charset.
        assert_eq!(
            err,
            DeedError::InvalidVersionSpec {
                line: 2,
                value: "not a version".into()
            }
        );
    }

    #[test]
    fn non_bullet_line_inside_section_is_illegal() {
        let deed = "### Dynamic Alterations\nuv is great\n";
        let err = parse_deed_alterations(deed).unwrap_err();
        assert_eq!(
            err,
            DeedError::IllegalLine {
                line: 2,
                text: "uv is great".into()
            }
        );
    }

    #[test]
    fn duplicate_tool_in_one_section_is_refused() {
        let deed = "### Dynamic Alterations\n- pixi uv\n- pixi uv = 0.5\n";
        let err = parse_deed_alterations(deed).unwrap_err();
        assert_eq!(
            err,
            DeedError::DuplicateTool {
                line: 3,
                kind: ToolKind::Pixi,
                name: "uv".into()
            }
        );
    }

    #[test]
    fn upsert_replaces_existing_and_appends_new_in_order() {
        let base = blueprint_toml(
            2,
            r#"[[tools]]
kind = "pixi"
name = "uv"
version_spec = "0.3.0"

[[tools]]
kind = "apk"
name = "jq"
"#,
        );
        let deed = "### Dynamic Alterations\n- pixi uv = 0.4.2\n- nvm node = 22\n";
        let alterations = parse_deed_alterations(deed).unwrap();
        let next = apply_alterations(&base, &alterations).unwrap();

        assert_eq!(next.meta().version(), 3, "version bumps by one");
        assert_eq!(next.tools().len(), 3);
        // Existing entry replaced in place (declaration order preserved).
        assert_eq!(next.tools()[0].name(), "uv");
        assert_eq!(next.tools()[0].version_spec(), Some("0.4.2"));
        // New tool appended last, in declaration order.
        assert_eq!(next.tools()[2].kind(), ToolKind::Nvm);
        assert_eq!(next.tools()[2].version_spec(), Some("22"));
        // Immutability: the base is untouched.
        assert_eq!(base.meta().version(), 2);
        assert_eq!(base.tools()[0].version_spec(), Some("0.3.0"));
    }

    #[test]
    fn version_overflow_is_refused_at_max() {
        let at_max = blueprint_toml(u32::MAX, "");
        let deed = "### Dynamic Alterations\n- pixi uv\n";
        let parsed = parse_deed_alterations(deed).unwrap();
        let err = apply_alterations(&at_max, &parsed).unwrap_err();
        assert!(matches!(err, BlueprintError::VersionOverflow { .. }));
    }

    #[test]
    fn promotion_request_validates_like_a_decree_freight_path() {
        let request =
            PromotionRequest::new(7, "courier/freight/7/deliverable.tar", "ledger-v1").unwrap();
        assert_eq!(request.commission(), 7);
        assert_eq!(request.freight_path().components().len(), 4);
        assert_eq!(request.artifact_name().as_str(), "ledger-v1");

        // The freight directory itself (2 components) is not a file.
        let err = PromotionRequest::new(7, "courier/freight", "x").unwrap_err();
        assert!(matches!(err, PromotionError::NotAFreightFile { .. }));

        // Outside the freight prefix.
        let err =
            PromotionRequest::new(7, "guild/blueprints/village-alpine.toml", "x").unwrap_err();
        assert!(matches!(err, PromotionError::NotAFreightFile { .. }));

        // Traversal is refused at the TreeRelPath boundary.
        let err = PromotionRequest::new(7, "courier/freight/../../etc/passwd", "x").unwrap_err();
        assert!(matches!(err, PromotionError::FreightPath(_)));

        // Artifact name sanitizes.
        let err = PromotionRequest::new(7, "courier/freight/7/x.tar", "../evil").unwrap_err();
        assert!(matches!(err, PromotionError::InvalidArtifactName { .. }));
    }

    #[test]
    fn baseline_entry_is_skipped_as_environment_floor() {
        let deed = r#"# DEED — environment record · parcel1001

- identity: worker
- uid slot: 1001
- commission: 1
- blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24

Tool installs and environment mutations are recorded here.

### Dynamic Alterations
- baseline · toolchain bootstrap · uid 1001 · image localhost/hamlet/village-alpine:v1
  - manifest: /home/1001/.hamlet/manifest.json · sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
  - tool: pixi/pixi 0.34.* → 0.34.0 · pixi@0.34.0 sha256 0123
  - image layer (apk): uutils-coreutils, git, acl, ca-certificates, curl
  - env keys: PIXI_CACHE_DIR, LANG (values never recorded — SPEC §11.4)

- pixi uv = 0.4.2
- cargo ripgrep
"#;
        let alterations = parse_deed_alterations(deed).unwrap();
        assert_eq!(alterations.len(), 2);
        assert_eq!(alterations[0].name(), "uv");
        assert_eq!(alterations[1].name(), "ripgrep");

        let bp_ref = parse_deed_blueprint_ref(deed).unwrap();
        assert_eq!(bp_ref.as_str(), "village-alpine");
    }

    #[test]
    fn already_merged_and_digest_truth_table() {
        let bp = blueprint_toml(
            1,
            r#"[[tools]]
kind = "pixi"
name = "uv"
version_spec = "0.4.2"
"#,
        );
        let alts_same = vec![Alteration {
            kind: ToolKind::Pixi,
            name: "uv".into(),
            version_spec: Some("0.4.2".into()),
        }];
        assert!(is_already_merged(&bp, &alts_same));

        let alts_diff_ver = vec![Alteration {
            kind: ToolKind::Pixi,
            name: "uv".into(),
            version_spec: Some("0.5.0".into()),
        }];
        assert!(!is_already_merged(&bp, &alts_diff_ver));

        let alts_new_tool = vec![Alteration {
            kind: ToolKind::Cargo,
            name: "ripgrep".into(),
            version_spec: None,
        }];
        assert!(!is_already_merged(&bp, &alts_new_tool));

        let d1 = alterations_digest(&alts_same);
        let d2 = alterations_digest(&alts_same);
        assert_eq!(d1, d2, "digest is deterministic");
        assert_eq!(d1.len(), 16);
    }
}
