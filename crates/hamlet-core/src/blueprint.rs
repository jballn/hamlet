//! Blueprint schema (SPEC §5; schema was [DEV] → resolved by ADR-0009).
//!
//! Immutable versioned TOML environment declarations in `guild/blueprints/`
//! (tools + versions: pixi packages, node versions, cargo toolchains,
//! apk additions). Parsed here; executed by the daemon at parcel
//! provisioning. Also defines the parse target for blueprint evolution
//! write-back (SPEC §9.4, see [`crate::promote`]).
//!
//! # Schema (ADR-0009)
//!
//! ```toml
//! [meta]
//! name = "village-alpine"          # SafeName (CONVENTIONS §4)
//! version = 1                      # >= 1; bumped by Sprint 06 promotion
//! base_image = "docker.io/library/alpine:3.24"
//!
//! [[tools]]
//! kind = "pixi"                    # pixi | nvm | cargo | apk
//! name = "python"                  # tool-name charset (ADR-0009)
//! version_spec = "3.12.*"          # optional; version-spec charset
//!
//! [env]                            # non-secret defaults only
//! PIXI_CACHE_DIR = "/var/pkgcache/pixi"
//!
//! [skills]                         # references into market/skills/
//! refs = ["code-review"]
//! ```
//!
//! `[meta]` is required; `tools`, `env`, `skills` default to empty.
//! Unknown fields are rejected everywhere (`deny_unknown_fields`).
//!
//! `base_image` is the PARENT image: the daemon derives the parcel image
//! from it (`localhost/hamlet/<name>:v<version>-f<floor>-c<fingerprint>`,
//! ADR-0012) and installs the `kind = "apk"` tools plus its own bootstrap
//! floor into that derived layer. `kind = pixi|nvm|cargo` tools are
//! installed into the parcel home at bootstrap, from the concrete
//! version+sha256 pins in [`crate::pins`] (ADR-0013) — an unpinned tool is
//! refused, never guessed at.
//!
//! `[env]` must not declare an installation prefix (`HOME`, `PATH`,
//! `CARGO_HOME`, `RUSTUP_HOME`, `PIXI_HOME`, `NVM_DIR`) or a dynamic-loader
//! variable (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `ENV`,
//! `BASH_ENV`, `IFS`): those are per-UID/structural and bootstrap refuses
//! them (ADR-0014 cl. 2–3).
//!
//! # Immutability
//!
//! A loaded [`Blueprint`] exposes **no `&mut` API surface** — fields are
//! private, there are no setters, and nothing returns mutable references.
//! The only mutation path is SPEC §9.4 promotion (Sprint 06): the daemon
//! parses DEED.md `### Dynamic Alterations` and constructs a *new*
//! `Blueprint` value with a bumped `meta.version`. In-place mutation is
//! unrepresentable by construction.
//!
//! # Errors
//!
//! Every validation failure carries the exact field path (`meta.name`,
//! `tools[2].version_spec`, `env.KEY`, `skills.refs[1]`) plus a typed reason
//! ([`BlueprintError`]); TOML syntax/type errors carry the `toml` crate's
//! message (which includes key path and line/column).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use crate::sanitize::{sanitize_name, SafeName, SanitizeError};

/// The committed default blueprint (SPEC §4 base: Alpine + uutils +
/// Pixi/NVM/Cargo), shipped at `defaults/village-alpine.toml`. Seeded into
/// `guild/blueprints/` by provisioning; kept in sync by the golden test
/// [`tests::default_blueprint_parses`].
pub const DEFAULT_BLUEPRINT_TOML: &str = include_str!("../../../defaults/village-alpine.toml");

/// Maximum length of a tool name / version spec / env value (ADR-0009).
pub(crate) const MAX_TOOL_NAME_LEN: usize = 64;
pub(crate) const MAX_VERSION_SPEC_LEN: usize = 128;
const MAX_BASE_IMAGE_LEN: usize = 256;
const MAX_ENV_VALUE_LEN: usize = 4096;

pub(crate) fn tool_name_re() -> &'static Regex {
    // Broader than SafeName (apk allows '+', e.g. `g++`), but still
    // shell-safe: ASCII allowlist, must start alphanumeric, no separators.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$").unwrap())
}

pub(crate) fn version_spec_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:\*|[A-Za-z0-9][A-Za-z0-9._+*^~=<>,-]*)$").unwrap())
}

fn base_image_re() -> &'static Regex {
    // Docker/OCI image reference charset, must start alphanumeric.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/@:-]*$").unwrap())
}

fn env_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap())
}

fn secret_key_re() -> &'static Regex {
    // [env] is non-secret by contract (SPEC §11.4: secrets live only in
    // .env). Reject key names that look like credential carriers.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(key|secret|token|passw|credential)").unwrap())
}

/// Tool provisioning channel (SPEC §4: user-space multiplexers + apk).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// pixi package installed into the parcel's pixi workspace.
    Pixi,
    /// Node.js runtime installed via nvm (`name = "node"`,
    /// `version_spec` = node version or LTS line).
    Nvm,
    /// Rust toolchain installed via rustup (`name = "rust"`,
    /// `version_spec` = channel, e.g. `stable` or `1.79.0`).
    Cargo,
    /// Alpine system package installed via apk.
    Apk,
}

impl ToolKind {
    /// Lowercase wire name as it appears in TOML.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pixi => "pixi",
            Self::Nvm => "nvm",
            Self::Cargo => "cargo",
            Self::Apk => "apk",
        }
    }
}

impl fmt::Display for ToolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `[[tools]]` entry: what to provision, through which channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    kind: ToolKind,
    name: String,
    version_spec: Option<String>,
}

impl ToolSpec {
    /// Provisioning channel.
    #[must_use]
    pub fn kind(&self) -> ToolKind {
        self.kind
    }

    /// Validated tool name (ADR-0009 charset).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Optional validated version spec; `None` = channel default (latest
    /// LTS / stable / apk pinned-by-image).
    #[must_use]
    pub fn version_spec(&self) -> Option<&str> {
        self.version_spec.as_deref()
    }
}

/// `[meta]` block: identity + base image of the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueprintMeta {
    name: SafeName,
    version: u32,
    base_image: String,
}

impl BlueprintMeta {
    /// Blueprint name — must equal the file stem in `guild/blueprints/`.
    #[must_use]
    pub fn name(&self) -> &SafeName {
        &self.name
    }

    /// Content version, `>= 1`; bumped only by the Sprint 06 promotion path.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Validated container base image reference (SPEC §4: slim Alpine).
    #[must_use]
    pub fn base_image(&self) -> &str {
        &self.base_image
    }
}

/// Typed validation failure with the exact offending field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlueprintError {
    /// TOML syntax, wrong type, unknown field, or missing required field —
    /// `message` is the `toml` crate's text (includes key path + position).
    Syntax { message: String },
    /// `meta.version` was 0 (versions start at 1).
    VersionZero { path: String },
    /// `meta.name` / `skills[i]` failed the CONVENTIONS §4 sanitizer.
    InvalidName { path: String, source: SanitizeError },
    /// `meta.base_image` violates the image-reference charset/length.
    InvalidBaseImage { path: String, value: String },
    /// `tools[i].kind` is not one of pixi|nvm|cargo|apk.
    UnknownKind { path: String, value: String },
    /// `tools[i].name` violates the tool-name charset/length (ADR-0009).
    InvalidToolName { path: String, value: String },
    /// `tools[i].version_spec` violates the version-spec charset/length.
    InvalidVersionSpec { path: String, value: String },
    /// Two `[[tools]]` entries share the same (kind, name).
    DuplicateTool { path: String, name: String },
    /// `[env]` key is not a valid environment identifier.
    InvalidEnvKey { path: String, value: String },
    /// `[env]` key looks like a secret carrier — secrets belong in `.env`
    /// only (SPEC §11.4), never in a committed blueprint.
    ForbiddenEnvKey { path: String, value: String },
    /// `[env]` value contains control characters or exceeds the length cap.
    InvalidEnvValue { path: String },
    /// Two `[skills]` entries reference the same skill.
    DuplicateSkill { path: String, name: String },
}

impl BlueprintError {
    /// The offending field path, if the error is field-scoped
    /// (`None` for whole-document TOML syntax errors).
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Syntax { .. } => None,
            Self::VersionZero { path }
            | Self::InvalidName { path, .. }
            | Self::InvalidBaseImage { path, .. }
            | Self::UnknownKind { path, .. }
            | Self::InvalidToolName { path, .. }
            | Self::InvalidVersionSpec { path, .. }
            | Self::DuplicateTool { path, .. }
            | Self::InvalidEnvKey { path, .. }
            | Self::ForbiddenEnvKey { path, .. }
            | Self::InvalidEnvValue { path }
            | Self::DuplicateSkill { path, .. } => Some(path),
        }
    }
}

impl fmt::Display for BlueprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { message } => write!(f, "blueprint syntax error: {message}"),
            Self::VersionZero { path } => write!(f, "{path}: version must be >= 1"),
            Self::InvalidName { path, source } => write!(f, "{path}: invalid name: {source}"),
            Self::InvalidBaseImage { path, value } => {
                write!(f, "{path}: invalid image reference {value:?}")
            }
            Self::UnknownKind { path, value } => write!(
                f,
                "{path}: unknown tool kind {value:?} (expected pixi|nvm|cargo|apk)"
            ),
            Self::InvalidToolName { path, value } => {
                write!(f, "{path}: invalid tool name {value:?}")
            }
            Self::InvalidVersionSpec { path, value } => {
                write!(f, "{path}: invalid version spec {value:?}")
            }
            Self::DuplicateTool { path, name } => write!(f, "{path}: duplicate tool {name:?}"),
            Self::InvalidEnvKey { path, value } => {
                write!(f, "{path}: invalid environment identifier {value:?}")
            }
            Self::ForbiddenEnvKey { path, value } => write!(
                f,
                "{path}: key {value:?} looks secret-bearing; [env] holds non-secret defaults only"
            ),
            Self::InvalidEnvValue { path } => {
                write!(
                    f,
                    "{path}: value has control characters or exceeds {MAX_ENV_VALUE_LEN} chars"
                )
            }
            Self::DuplicateSkill { path, name } => write!(f, "{path}: duplicate skill {name:?}"),
        }
    }
}

impl std::error::Error for BlueprintError {}

// --- private wire types (serde) -------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlueprint {
    meta: RawMeta,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    skills: RawSkills,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkills {
    #[serde(default)]
    refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeta {
    name: String,
    version: u32,
    base_image: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    kind: String,
    name: String,
    #[serde(default)]
    version_spec: Option<String>,
}

/// A parsed, validated, **immutable** blueprint (SPEC §5, ADR-0009).
///
/// No setter, no `&mut` method, no public field exists: once loaded by the
/// daemon a blueprint can only be replaced wholesale (new parse) or evolved
/// by the Sprint 06 promotion path constructing a fresh value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blueprint {
    meta: BlueprintMeta,
    tools: Vec<ToolSpec>,
    env: BTreeMap<String, String>,
    skills: Vec<SafeName>,
}

impl Blueprint {
    /// Parse and validate TOML text into a typed blueprint.
    ///
    /// Pure: the caller (daemon/CLI) reads the file; core never touches
    /// the filesystem here (ADR-0001 purity seam).
    ///
    /// # Errors
    ///
    /// Returns [`BlueprintError::Syntax`] for TOML-level problems and a
    /// path-scoped variant for the first semantic violation found
    /// (validation order: meta → tools → env → skills).
    pub fn from_toml(text: &str) -> Result<Self, BlueprintError> {
        let raw: RawBlueprint = toml::from_str(text).map_err(|e| BlueprintError::Syntax {
            message: e.to_string(),
        })?;

        let meta = validate_meta(raw.meta)?;
        let tools = validate_tools(raw.tools)?;
        let env = validate_env(raw.env)?;
        let skills = validate_skills(raw.skills.refs)?;
        Ok(Self {
            meta,
            tools,
            env,
            skills,
        })
    }

    /// Parse the committed default blueprint ([`DEFAULT_BLUEPRINT_TOML`]).
    ///
    /// # Errors
    ///
    /// Errors only if the committed default itself is invalid — a CI-level
    /// defect, guarded by [`tests::default_blueprint_parses`].
    pub fn default_village_alpine() -> Result<Self, BlueprintError> {
        Self::from_toml(DEFAULT_BLUEPRINT_TOML)
    }

    /// The `[meta]` block.
    #[must_use]
    pub fn meta(&self) -> &BlueprintMeta {
        &self.meta
    }

    /// `[[tools]]` entries in declaration order.
    #[must_use]
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// `[env]` non-secret defaults, sorted by key (state contract §3.1).
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// `[skills]` references into `market/skills/`, declaration order.
    #[must_use]
    pub fn skills(&self) -> &[SafeName] {
        &self.skills
    }
}

fn validate_meta(raw: RawMeta) -> Result<BlueprintMeta, BlueprintError> {
    let name = sanitize_name(&raw.name).map_err(|source| BlueprintError::InvalidName {
        path: "meta.name".into(),
        source,
    })?;
    if raw.version == 0 {
        return Err(BlueprintError::VersionZero {
            path: "meta.version".into(),
        });
    }
    if raw.base_image.len() > MAX_BASE_IMAGE_LEN || !base_image_re().is_match(&raw.base_image) {
        return Err(BlueprintError::InvalidBaseImage {
            path: "meta.base_image".into(),
            value: raw.base_image,
        });
    }
    Ok(BlueprintMeta {
        name,
        version: raw.version,
        base_image: raw.base_image,
    })
}

fn validate_tools(raw: Vec<RawTool>) -> Result<Vec<ToolSpec>, BlueprintError> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (i, tool) in raw.into_iter().enumerate() {
        let path = |field: &str| format!("tools[{i}].{field}");
        let kind = match tool.kind.as_str() {
            "pixi" => ToolKind::Pixi,
            "nvm" => ToolKind::Nvm,
            "cargo" => ToolKind::Cargo,
            "apk" => ToolKind::Apk,
            other => {
                return Err(BlueprintError::UnknownKind {
                    path: path("kind"),
                    value: other.to_owned(),
                })
            }
        };
        if tool.name.len() > MAX_TOOL_NAME_LEN || !tool_name_re().is_match(&tool.name) {
            return Err(BlueprintError::InvalidToolName {
                path: path("name"),
                value: tool.name,
            });
        }
        let version_spec = match tool.version_spec {
            None => None,
            Some(spec) => {
                if spec.len() > MAX_VERSION_SPEC_LEN || !version_spec_re().is_match(&spec) {
                    return Err(BlueprintError::InvalidVersionSpec {
                        path: path("version_spec"),
                        value: spec,
                    });
                }
                Some(spec)
            }
        };
        if !seen.insert((kind, tool.name.clone())) {
            return Err(BlueprintError::DuplicateTool {
                path: path("name"),
                name: tool.name,
            });
        }
        out.push(ToolSpec {
            kind,
            name: tool.name,
            version_spec,
        });
    }
    Ok(out)
}

fn validate_env(raw: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, BlueprintError> {
    for (key, value) in &raw {
        let path = format!("env.{key}");
        if !env_key_re().is_match(key) {
            return Err(BlueprintError::InvalidEnvKey {
                path,
                value: key.clone(),
            });
        }
        if secret_key_re().is_match(key) {
            return Err(BlueprintError::ForbiddenEnvKey {
                path,
                value: key.clone(),
            });
        }
        if value.len() > MAX_ENV_VALUE_LEN || value.chars().any(char::is_control) {
            return Err(BlueprintError::InvalidEnvValue { path });
        }
    }
    Ok(raw)
}

fn validate_skills(raw: Vec<String>) -> Result<Vec<SafeName>, BlueprintError> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (i, skill) in raw.into_iter().enumerate() {
        let path = format!("skills.refs[{i}]");
        let name = sanitize_name(&skill).map_err(|source| BlueprintError::InvalidName {
            path: path.clone(),
            source,
        })?;
        if !seen.insert(name.as_str().to_owned()) {
            return Err(BlueprintError::DuplicateSkill { path, name: skill });
        }
        out.push(name);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Blueprint, BlueprintError> {
        Blueprint::from_toml(text)
    }

    fn err(text: &str) -> BlueprintError {
        parse(text).expect_err("expected a validation error")
    }

    const MINIMAL: &str = r#"
[meta]
name = "minimal"
version = 1
base_image = "docker.io/library/alpine:3.20"
"#;

    /// Golden vector: the committed default blueprint parses, and every
    /// SPEC §4 baseline element is present.
    #[test]
    fn default_blueprint_parses() {
        let bp = Blueprint::default_village_alpine().expect("committed default must parse");
        assert_eq!(bp.meta().name().as_str(), "village-alpine");
        assert_eq!(bp.meta().version(), 1);
        assert_eq!(bp.meta().base_image(), "docker.io/library/alpine:3.24");

        let has = |k: ToolKind, n: &str| bp.tools().iter().any(|t| t.kind() == k && t.name() == n);
        assert!(
            has(ToolKind::Apk, "uutils-coreutils"),
            "SPEC §4: Alpine + uutils (the real Alpine package name — `uutils` \
             exists in no 3.2x repo; verified against dl-cdn.alpinelinux.org)"
        );
        assert!(has(ToolKind::Apk, "git"), "commission work needs git");
        assert!(has(ToolKind::Apk, "acl"), "SPEC §4: freight ACL tooling");
        assert!(has(ToolKind::Pixi, "pixi"), "SPEC §4: Pixi bundled");
        assert!(has(ToolKind::Nvm, "node"), "SPEC §4: NVM bundled");
        assert!(has(ToolKind::Cargo, "rust"), "SPEC §4: Cargo bundled");

        // [env] defaults steer package *caches* into the pkgcache mount.
        assert_eq!(
            bp.env().get("PIXI_CACHE_DIR").map(String::as_str),
            Some("/var/pkgcache/pixi")
        );
        assert_eq!(
            bp.env().get("NPM_CONFIG_CACHE").map(String::as_str),
            Some("/var/pkgcache/npm")
        );
        // Installation prefixes are per-UID and derived by bootstrap; a
        // blueprint that declares one is refused at bootstrap time
        // (ADR-0014 cl. 2–3) — the default must not ship that footgun.
        for structural in [
            "CARGO_HOME",
            "RUSTUP_HOME",
            "PIXI_HOME",
            "NVM_DIR",
            "HOME",
            "PATH",
        ] {
            assert!(
                !bp.env().contains_key(structural),
                "{structural} is bootstrap-derived, never blueprint-declared"
            );
        }
        assert!(bp.skills().is_empty(), "fresh villages have no skills");
    }

    /// Golden vector: full-schema parse, exact typed expectations.
    #[test]
    fn full_schema_golden() {
        let text = r#"
[meta]
name = "forge.dev"
version = 7
base_image = "localhost/hamlet/forge:1.2.3"

[[tools]]
kind = "apk"
name = "g++"

[[tools]]
kind = "pixi"
name = "python"
version_spec = "3.12.*"

[[tools]]
kind = "nvm"
name = "node"
version_spec = "20"

[[tools]]
kind = "cargo"
name = "rust"
version_spec = "stable"

[env]
EDITOR = "vi"
LANG = "C.UTF-8"

[skills]
refs = ["code-review", "pdf-tools"]
"#;
        let bp = parse(text).expect("valid full-schema blueprint");
        assert_eq!(bp.meta().name().as_str(), "forge.dev");
        assert_eq!(bp.meta().version(), 7);
        assert_eq!(bp.tools().len(), 4);
        assert_eq!(bp.tools()[0].kind(), ToolKind::Apk);
        assert_eq!(bp.tools()[0].name(), "g++");
        assert_eq!(bp.tools()[0].version_spec(), None);
        assert_eq!(bp.tools()[1].version_spec(), Some("3.12.*"));
        let env: Vec<_> = bp.env().keys().collect();
        assert_eq!(env, vec!["EDITOR", "LANG"], "BTreeMap sorts keys");
        let skills: Vec<_> = bp.skills().iter().map(SafeName::as_str).collect();
        assert_eq!(skills, vec!["code-review", "pdf-tools"]);
    }

    #[test]
    fn optional_sections_default_empty() {
        let bp = parse(MINIMAL).expect("meta-only blueprint");
        assert!(bp.tools().is_empty() && bp.env().is_empty() && bp.skills().is_empty());
    }

    /// Golden error vectors: each violation reports the exact field path.
    #[test]
    fn error_paths_are_precise() {
        assert_eq!(
            err(&MINIMAL.replace("version = 1", "version = 0")),
            BlueprintError::VersionZero {
                path: "meta.version".into()
            }
        );
        assert_eq!(
            err(&MINIMAL.replace(r#"name = "minimal""#, r#"name = "../escape""#)),
            BlueprintError::InvalidName {
                path: "meta.name".into(),
                source: SanitizeError::PathSeparator
            }
        );
        assert_eq!(
            err(&MINIMAL.replace("alpine:3.20", "alpine 3.20")),
            BlueprintError::InvalidBaseImage {
                path: "meta.base_image".into(),
                value: "docker.io/library/alpine 3.20".into()
            }
        );
        let bad_kind = format!("{MINIMAL}\n[[tools]]\nkind = \"brew\"\nname = \"x\"\n");
        assert_eq!(
            err(&bad_kind),
            BlueprintError::UnknownKind {
                path: "tools[0].kind".into(),
                value: "brew".into()
            }
        );
        let bad_tool = format!("{MINIMAL}\n[[tools]]\nkind = \"apk\"\nname = \"ok\"\n\n[[tools]]\nkind = \"apk\"\nname = \"-rm-rf\"\n");
        assert_eq!(
            err(&bad_tool),
            BlueprintError::InvalidToolName {
                path: "tools[1].name".into(),
                value: "-rm-rf".into()
            }
        );
        let bad_spec =
            format!("{MINIMAL}\n[[tools]]\nkind = \"pixi\"\nname = \"p\"\nversion_spec = \"1.0; rm -rf /\"\n");
        assert_eq!(
            err(&bad_spec),
            BlueprintError::InvalidVersionSpec {
                path: "tools[0].version_spec".into(),
                value: "1.0; rm -rf /".into()
            }
        );
        let dup_tool =
            format!("{MINIMAL}\n[[tools]]\nkind = \"apk\"\nname = \"git\"\n\n[[tools]]\nkind = \"apk\"\nname = \"git\"\n");
        assert_eq!(
            err(&dup_tool),
            BlueprintError::DuplicateTool {
                path: "tools[1].name".into(),
                name: "git".into()
            }
        );
        let secret = format!("{MINIMAL}\n[env]\nHAMLET_API_KEY = \"hunter2\"\n");
        assert_eq!(
            err(&secret),
            BlueprintError::ForbiddenEnvKey {
                path: "env.HAMLET_API_KEY".into(),
                value: "HAMLET_API_KEY".into()
            }
        );
        let bad_key = format!("{MINIMAL}\n[env]\n\"2FAST\" = \"x\"\n");
        assert_eq!(
            err(&bad_key),
            BlueprintError::InvalidEnvKey {
                path: "env.2FAST".into(),
                value: "2FAST".into()
            }
        );
        let bad_skill = format!("{MINIMAL}\n[skills]\nrefs = [\"a/b\"]\n");
        assert_eq!(
            err(&bad_skill),
            BlueprintError::InvalidName {
                path: "skills.refs[0]".into(),
                source: SanitizeError::PathSeparator
            }
        );
        let dup_skill = format!("{MINIMAL}\n[skills]\nrefs = [\"a\", \"a\"]\n");
        assert_eq!(
            err(&dup_skill),
            BlueprintError::DuplicateSkill {
                path: "skills.refs[1]".into(),
                name: "a".into()
            }
        );
    }

    #[test]
    fn syntax_errors_carry_toml_detail() {
        let e = err("[meta]\nname = 42\n");
        match e {
            BlueprintError::Syntax { message } => {
                assert!(
                    message.contains("meta.name") || message.contains("name"),
                    "{message}"
                );
            }
            other => panic!("expected Syntax, got {other:?}"),
        }
        // Unknown fields are rejected everywhere.
        let e = err("[meta]\nname = \"m\"\nversion = 1\nbase_image = \"a:1\"\nshiny = true\n");
        assert!(matches!(e, BlueprintError::Syntax { .. }), "{e:?}");
        let e = err(&format!(
            "{MINIMAL}\n[[tools]]\nkind = \"apk\"\nname = \"x\"\nflavor = \"hot\"\n"
        ));
        assert!(matches!(e, BlueprintError::Syntax { .. }), "{e:?}");
    }

    #[test]
    fn missing_meta_is_syntax_error() {
        let e = err("");
        assert!(matches!(e, BlueprintError::Syntax { .. }), "{e:?}");
    }

    /// Immutability is structural: this test documents that the public API
    /// offers no mutation — only whole-value replacement via `from_toml`.
    /// (Compile-fail surface: no `&mut self` method exists on Blueprint.)
    #[test]
    fn evolution_is_replace_not_mutate() {
        let v1 = parse(MINIMAL).unwrap();
        let v2 = parse(&MINIMAL.replace("version = 1", "version = 2")).unwrap();
        assert_ne!(v1, v2, "a promoted blueprint is a distinct value");
        assert_eq!(v1.meta().version(), 1, "the old value is untouched");
    }
}
