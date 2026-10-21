//! Toolchain pins (ADR-0012 cl. 4, ADR-0013): the artifact trust anchor for
//! blueprint bootstrap.
//!
//! A blueprint declares *what* a parcel needs (`[[tools]]` with a
//! `version_spec`); this table declares *exactly which bytes* satisfy it:
//! concrete upstream version, URL, host arch, archive format and **sha256**.
//! The daemon's bootstrap ([`hamlet-daemon`](https://docs.rs/hamlet-daemon)
//! `bootstrap` module) refuses to install a tool that has no pin here, and
//! refuses to extract or execute any artifact whose digest does not match.
//!
//! # Why digests are mandatory, not decorative
//!
//! `market/pkgcache/` is the shared download mirror every parcel uses and is
//! mode **01777** (ADR-0011) — any agent UID may create or replace files in
//! it. A cache-hit artifact is therefore *untrusted input*: without a digest
//! check, one compromised agent could hand a poisoned `cargo` tarball to
//! every parcel provisioned afterwards (cross-UID code execution through the
//! shared cache). Pins are committed in-repo, so the trust anchor is the
//! reviewed source tree, not the cache (ADR-0013 cl. 2).
//!
//! # Purity
//!
//! Parsing takes `&str`; lookups take values. No filesystem, process or
//! network access (CONVENTIONS §2 purity seam) — the daemon injects the
//! cached-artifact state it observed and gets a plan back.
//!
//! # Schema
//!
//! ```toml
//! version = 1                        # PINS_SCHEMA_VERSION; unknown = refusal
//!
//! [[pins]]
//! kind = "cargo"                     # ToolKind (blueprint.rs); apk is never pinned
//! name = "rust"                      # must equal the blueprint tool name
//! version_spec = "stable"            # must equal the blueprint version_spec
//! version = "1.98.1"                 # the concrete frozen version
//!
//!   [[pins.artifacts]]
//!   role = "rustc"                   # unique within the pin; install order
//!   version = "1.98.1"
//!   arch = "x86_64"                  # matched against env::consts::ARCH
//!   format = "tar.xz"                # tar.gz | tar.xz | binary
//!   file = "rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz"
//!   url = "https://static.rust-lang.org/dist/…"
//!   sha256 = "dea91616…"             # 64 lowercase hex
//! ```
//!
//! All levels `deny_unknown_fields`. A floating spec (`stable`, `22`) is
//! frozen to the concrete `version`: refreshing a toolchain means editing
//! `defaults/toolchain-pins.toml` in a reviewed commit, never a silent
//! upstream drift.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use crate::blueprint::{
    tool_name_re, version_spec_re, ToolKind, MAX_TOOL_NAME_LEN, MAX_VERSION_SPEC_LEN,
};

/// The committed pin table (`defaults/toolchain-pins.toml`), embedded so a
/// daemon binary carries its own trust anchor. Kept in sync by the golden
/// test `tests::default_pins_parse`.
pub const DEFAULT_TOOLCHAIN_PINS_TOML: &str = include_str!("../../../defaults/toolchain-pins.toml");

/// Only schema version this parser accepts (additive migrations bump it).
pub const PINS_SCHEMA_VERSION: u32 = 1;

/// How an artifact's bytes are packaged — decides the in-container
/// extraction argv the daemon renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum ArtifactFormat {
    /// gzip'd tarball (`tar -xzf`).
    #[serde(rename = "tar.gz")]
    TarGz,
    /// xz'd tarball (`tar -xJf`).
    #[serde(rename = "tar.xz")]
    TarXz,
    /// Bare executable/image file (no extraction; `chmod` + run).
    #[serde(rename = "binary")]
    Binary,
}

impl ArtifactFormat {
    /// Wire name as it appears in TOML.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::Binary => "binary",
        }
    }

    /// `tar` extraction flag (`-z` gzip, `-J` xz); `None` for a bare binary.
    #[must_use]
    pub fn tar_flag(&self) -> Option<&'static str> {
        match self {
            Self::TarGz => Some("-z"),
            Self::TarXz => Some("-J"),
            Self::Binary => None,
        }
    }
}

impl fmt::Display for ArtifactFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One pinned download: the exact bytes that satisfy one `role` of a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPin {
    role: String,
    version: String,
    arch: String,
    format: ArtifactFormat,
    file: String,
    url: String,
    sha256: String,
}

impl ArtifactPin {
    /// Role within the tool (e.g. `manager`/`runtime`, `rustc`/`cargo`);
    /// unique per pin and the order the daemon installs them in.
    #[must_use]
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Concrete upstream version of *this* artifact (may differ from the
    /// tool version: nvm 0.40.8 ships node 22.23.3).
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Host architecture the bytes were built for.
    #[must_use]
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// Packaging (extraction argv selector).
    #[must_use]
    pub fn format(&self) -> ArtifactFormat {
        self.format
    }

    /// Cache/staging file name (basename only; charset-validated, never a
    /// path — the daemon derives directories from typed tree accessors).
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Upstream URL (always `https://`).
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Expected sha256, 64 lowercase hex digits.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// One pinned tool: the `(kind, name, version_spec)` triple a blueprint
/// `[[tools]]` entry must match, plus the frozen version and its artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPin {
    kind: ToolKind,
    name: String,
    version_spec: String,
    version: String,
    artifacts: Vec<ArtifactPin>,
}

impl ToolPin {
    /// Provisioning channel this pin serves.
    #[must_use]
    pub fn kind(&self) -> ToolKind {
        self.kind
    }

    /// Blueprint tool name this pin answers to.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Blueprint `version_spec` this pin freezes (exact string match; an
    /// absent spec matches the empty string).
    #[must_use]
    pub fn version_spec(&self) -> &str {
        &self.version_spec
    }

    /// Concrete frozen version installed into the parcel home.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Artifacts in install order (never empty).
    #[must_use]
    pub fn artifacts(&self) -> &[ArtifactPin] {
        &self.artifacts
    }

    /// The artifact with `role`, if the pin declares one.
    #[must_use]
    pub fn artifact(&self, role: &str) -> Option<&ArtifactPin> {
        self.artifacts.iter().find(|a| a.role == role)
    }
}

/// Pin-table failure: every variant carries the exact field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// TOML syntax/type/unknown-field error. The `toml` crate's message
    /// already carries the key path and line/column.
    Toml {
        /// Crate message.
        message: String,
    },
    /// `version` is not [`PINS_SCHEMA_VERSION`].
    UnsupportedSchema {
        /// Always `version`.
        path: String,
        /// The value found.
        value: u32,
    },
    /// The table declares no pins — a truncated/empty file would silently
    /// make every tool unpinnable, so it is refused instead.
    EmptyTable,
    /// Two `[[pins]]` entries share `(kind, name, version_spec)`.
    DuplicatePin {
        /// `pins[i]`.
        path: String,
        /// Offending kind.
        kind: ToolKind,
        /// Offending name.
        name: String,
        /// Offending spec.
        version_spec: String,
    },
    /// Two artifacts of one pin share a `role`.
    DuplicateRole {
        /// `pins[i].artifacts[j]`.
        path: String,
        /// The repeated role.
        role: String,
    },
    /// A pin declares no artifacts.
    NoArtifacts {
        /// `pins[i]`.
        path: String,
    },
    /// A value failed its charset/shape rule.
    BadValue {
        /// Field path (`pins[2].artifacts[0].file`).
        path: String,
        /// The offending value.
        value: String,
        /// What was required.
        expected: &'static str,
    },
    /// A URL that is not `https://` — the first download is the only
    /// unverified byte source, so it must be TLS-authenticated.
    InsecureUrl {
        /// Field path.
        path: String,
        /// The offending URL.
        value: String,
    },
}

impl PinError {
    /// The field path this error points at, when it is field-scoped.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Toml { .. } => None,
            Self::UnsupportedSchema { path, .. }
            | Self::DuplicatePin { path, .. }
            | Self::DuplicateRole { path, .. }
            | Self::NoArtifacts { path }
            | Self::BadValue { path, .. }
            | Self::InsecureUrl { path, .. } => Some(path),
            Self::EmptyTable => None,
        }
    }
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml { message } => write!(f, "toolchain pins: {message}"),
            Self::UnsupportedSchema { path, value } => write!(
                f,
                "toolchain pins: {path} = {value} (this daemon speaks schema v{PINS_SCHEMA_VERSION})"
            ),
            Self::EmptyTable => write!(f, "toolchain pins: the table declares no [[pins]]"),
            Self::DuplicatePin {
                path,
                kind,
                name,
                version_spec,
            } => write!(
                f,
                "toolchain pins: {path}: duplicate pin for {kind}/{name} version_spec {version_spec:?}"
            ),
            Self::DuplicateRole { path, role } => {
                write!(f, "toolchain pins: {path}: duplicate artifact role {role:?}")
            }
            Self::NoArtifacts { path } => {
                write!(f, "toolchain pins: {path}: a pin must declare at least one artifact")
            }
            Self::BadValue {
                path,
                value,
                expected,
            } => write!(f, "toolchain pins: {path} = {value:?}: expected {expected}"),
            Self::InsecureUrl { path, value } => write!(
                f,
                "toolchain pins: {path} = {value:?}: only https:// URLs may seed the pkgcache"
            ),
        }
    }
}

impl std::error::Error for PinError {}

// --- wire types (private; validation happens on the way out) ---------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPins {
    version: u32,
    #[serde(default)]
    pins: Vec<RawPin>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPin {
    kind: ToolKind,
    name: String,
    #[serde(default)]
    version_spec: String,
    version: String,
    #[serde(default)]
    artifacts: Vec<RawArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtifact {
    role: String,
    version: String,
    arch: String,
    format: ArtifactFormat,
    file: String,
    url: String,
    sha256: String,
}

fn sha256_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-f]{64}$").unwrap())
}

/// Artifact file names and versions: `[A-Za-z0-9._+-]`, no leading `-`
/// (never an argv flag), no `..` (never a path), 1..=200 chars. Deliberately
/// wider than [`crate::sanitize`] because upstream release names carry
/// multiple dots (`node-v22.23.3-linux-x64-musl.tar.xz`) — but they are
/// still basenames only, and the daemon validates them again at argv build.
fn artifact_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._+-]{0,199}$").unwrap())
}

fn role_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,31}$").unwrap())
}

fn arch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9_]{1,32}$").unwrap())
}

/// URLs: ASCII, no whitespace or control characters (they land verbatim in a
/// `curl` argv slot), `https://` only.
fn url_ok(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() <= 2048
        && url.bytes().all(|b| b.is_ascii_graphic() || b == b'/')
        && !url.contains("..")
}

fn require(ok: bool, path: &str, value: &str, expected: &'static str) -> Result<(), PinError> {
    if ok {
        Ok(())
    } else {
        Err(PinError::BadValue {
            path: path.to_owned(),
            value: value.to_owned(),
            expected,
        })
    }
}

/// The pin table: an immutable, sorted `(kind, name, version_spec)` →
/// [`ToolPin`] map. No setter and no `&mut` API exists — refreshing pins
/// means parsing a new table (same immutability encoding as
/// [`crate::blueprint::Blueprint`], ADR-0009 cl. 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainPins {
    version: u32,
    pins: BTreeMap<(ToolKind, String, String), ToolPin>,
}

impl ToolchainPins {
    /// Parse and validate a pin table.
    ///
    /// # Errors
    ///
    /// [`PinError`] with the exact field path of the first violation.
    pub fn from_toml(text: &str) -> Result<Self, PinError> {
        let raw: RawPins = toml::from_str(text).map_err(|e| PinError::Toml {
            message: e.to_string(),
        })?;
        if raw.version != PINS_SCHEMA_VERSION {
            return Err(PinError::UnsupportedSchema {
                path: "version".to_owned(),
                value: raw.version,
            });
        }
        if raw.pins.is_empty() {
            return Err(PinError::EmptyTable);
        }
        let mut pins = BTreeMap::new();
        for (i, pin) in raw.pins.into_iter().enumerate() {
            let path = format!("pins[{i}]");
            let key = (pin.kind, pin.name.clone(), pin.version_spec.clone());
            if pins.contains_key(&key) {
                return Err(PinError::DuplicatePin {
                    path,
                    kind: key.0,
                    name: key.1,
                    version_spec: key.2,
                });
            }
            pins.insert(key, validate_pin(&path, pin)?);
        }
        Ok(Self {
            version: raw.version,
            pins,
        })
    }

    /// The committed default table (`defaults/toolchain-pins.toml`).
    ///
    /// # Errors
    ///
    /// [`PinError`] — the committed table is golden-tested, so this is
    /// effectively infallible; typed for symmetry with [`Self::from_toml`].
    pub fn default_hamlet() -> Result<Self, PinError> {
        Self::from_toml(DEFAULT_TOOLCHAIN_PINS_TOML)
    }

    /// Table schema version.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Resolve the pin for a blueprint tool. `version_spec` is matched
    /// exactly (`None` → the empty string): a floating spec is frozen by the
    /// table, never re-resolved against upstream.
    #[must_use]
    pub fn resolve(
        &self,
        kind: ToolKind,
        name: &str,
        version_spec: Option<&str>,
    ) -> Option<&ToolPin> {
        self.pins
            .get(&(kind, name.to_owned(), version_spec.unwrap_or("").to_owned()))
    }

    /// Every pin, sorted by `(kind, name, version_spec)`.
    pub fn tools(&self) -> impl Iterator<Item = &ToolPin> {
        self.pins.values()
    }

    /// Number of pins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// Whether the table is empty (never true for a parsed table —
    /// [`PinError::EmptyTable`] refuses those).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }
}

fn validate_pin(path: &str, raw: RawPin) -> Result<ToolPin, PinError> {
    // The pin KEY must be charset-identical to the blueprint fields it is
    // matched against (ADR-0009), or `resolve()` could never fire.
    require(
        raw.name.len() <= MAX_TOOL_NAME_LEN && tool_name_re().is_match(&raw.name),
        &format!("{path}.name"),
        &raw.name,
        "blueprint tool-name charset (ADR-0009)",
    )?;
    require(
        raw.version_spec.len() <= MAX_VERSION_SPEC_LEN
            && (raw.version_spec.is_empty() || version_spec_re().is_match(&raw.version_spec)),
        &format!("{path}.version_spec"),
        &raw.version_spec,
        "blueprint version-spec charset (ADR-0009), or empty",
    )?;
    require(
        artifact_name_re().is_match(&raw.version),
        &format!("{path}.version"),
        &raw.version,
        "artifact-name charset, no leading '-'",
    )?;
    if raw.artifacts.is_empty() {
        return Err(PinError::NoArtifacts {
            path: path.to_owned(),
        });
    }
    let mut seen_roles = BTreeSet::new();
    let mut artifacts = Vec::with_capacity(raw.artifacts.len());
    for (j, artifact) in raw.artifacts.into_iter().enumerate() {
        let apath = format!("{path}.artifacts[{j}]");
        if !seen_roles.insert(artifact.role.clone()) {
            return Err(PinError::DuplicateRole {
                path: apath,
                role: artifact.role,
            });
        }
        artifacts.push(validate_artifact(&apath, artifact)?);
    }
    Ok(ToolPin {
        kind: raw.kind,
        name: raw.name,
        version_spec: raw.version_spec,
        version: raw.version,
        artifacts,
    })
}

fn validate_artifact(path: &str, raw: RawArtifact) -> Result<ArtifactPin, PinError> {
    let name_re = artifact_name_re();
    require(
        role_re().is_match(&raw.role),
        &format!("{path}.role"),
        &raw.role,
        "lowercase [a-z0-9-], 1..=32, no leading '-'",
    )?;
    require(
        name_re.is_match(&raw.version),
        &format!("{path}.version"),
        &raw.version,
        "artifact-name charset, no leading '-'",
    )?;
    require(
        arch_re().is_match(&raw.arch),
        &format!("{path}.arch"),
        &raw.arch,
        "lowercase [a-z0-9_], 1..=32",
    )?;
    require(
        name_re.is_match(&raw.file) && !raw.file.contains(".."),
        &format!("{path}.file"),
        &raw.file,
        "basename: artifact-name charset, no '..', no leading '-'",
    )?;
    if !url_ok(&raw.url) {
        if !raw.url.starts_with("https://") {
            return Err(PinError::InsecureUrl {
                path: format!("{path}.url"),
                value: raw.url,
            });
        }
        return Err(PinError::BadValue {
            path: format!("{path}.url"),
            value: raw.url,
            expected: "ASCII https:// URL without whitespace, control chars or '..'",
        });
    }
    require(
        sha256_re().is_match(&raw.sha256),
        &format!("{path}.sha256"),
        &raw.sha256,
        "64 lowercase hex digits",
    )?;
    Ok(ArtifactPin {
        role: raw.role,
        version: raw.version,
        arch: raw.arch,
        format: raw.format,
        file: raw.file,
        url: raw.url,
        sha256: raw.sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins() -> ToolchainPins {
        ToolchainPins::default_hamlet().expect("committed pin table must parse")
    }

    /// Golden vector: the committed table parses and freezes exactly the
    /// SPEC §4 multiplexers at the versions/digests recorded in ADR-0013.
    #[test]
    fn default_pins_parse() {
        let p = pins();
        assert_eq!(p.version(), PINS_SCHEMA_VERSION);
        assert_eq!(p.len(), 3, "pixi, node (nvm), rust (cargo)");

        let pixi = p
            .resolve(ToolKind::Pixi, "pixi", Some("0.34.*"))
            .expect("pixi pin");
        assert_eq!(pixi.version(), "0.34.0");
        assert_eq!(pixi.artifacts().len(), 1);
        let a = pixi.artifact("primary").expect("primary artifact");
        assert_eq!(a.format(), ArtifactFormat::TarGz);
        assert_eq!(a.file(), "pixi-0.34.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(
            a.sha256(),
            "fbdec98dff8b522c4ceb12d76e3fdc177b55620a33451b350c94eae37b3803c8"
        );

        let node = p
            .resolve(ToolKind::Nvm, "node", Some("22"))
            .expect("node pin");
        assert_eq!(node.version(), "22.23.3");
        assert_eq!(
            node.artifacts()
                .iter()
                .map(|a| a.role())
                .collect::<Vec<_>>(),
            ["manager", "runtime"],
            "nvm first (creates $NVM_DIR), then the node runtime"
        );
        assert_eq!(node.artifact("manager").unwrap().version(), "0.40.8");
        assert_eq!(
            node.artifact("runtime").unwrap().format(),
            ArtifactFormat::TarXz,
            "musl node build: Alpine has no glibc"
        );
        assert!(
            node.artifact("runtime")
                .unwrap()
                .url()
                .contains("unofficial-builds.nodejs.org"),
            "official nodejs.org builds are glibc-only and cannot run on Alpine"
        );

        let rust = p
            .resolve(ToolKind::Cargo, "rust", Some("stable"))
            .expect("rust pin");
        assert_eq!(rust.version(), "1.98.1");
        assert_eq!(
            rust.artifacts()
                .iter()
                .map(|a| a.role())
                .collect::<Vec<_>>(),
            ["rustup", "rustc", "cargo", "rust-std"],
            "multiplexer first, then the toolchain components it links"
        );
        assert_eq!(
            rust.artifact("rustup").unwrap().format(),
            ArtifactFormat::Binary
        );
        assert_eq!(
            rust.artifact("rustup").unwrap().version(),
            "1.29.2",
            "the multiplexer carries its own upstream version, not the toolchain's"
        );
        for a in rust.artifacts() {
            assert_eq!(a.arch(), "x86_64");
            if a.role() != "rustup" {
                assert_eq!(a.version(), "1.98.1");
            }
        }
        // Every digest in the committed table is 64 lowercase hex.
        for pin in p.tools() {
            for a in pin.artifacts() {
                assert!(sha256_re().is_match(a.sha256()), "{}", a.file());
            }
        }
    }

    /// An absent `version_spec` matches the empty string, so a blueprint tool
    /// without a spec still resolves deterministically.
    #[test]
    fn resolve_matches_absent_spec_to_empty_string() {
        let p = ToolchainPins::from_toml(
            r#"version = 1
[[pins]]
kind = "pixi"
name = "pixi"
version = "0.34.0"
[[pins.artifacts]]
role = "primary"
version = "0.34.0"
arch = "x86_64"
format = "binary"
file = "pixi"
url = "https://example.invalid/pixi"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
"#,
        )
        .expect("spec-less pin");
        assert!(p.resolve(ToolKind::Pixi, "pixi", None).is_some());
        assert!(p.resolve(ToolKind::Pixi, "pixi", Some("0.34.*")).is_none());
        assert!(p.resolve(ToolKind::Apk, "pixi", None).is_none());
    }

    /// Unpinned tools resolve to `None` — the daemon turns that into a
    /// refusal, never a best-effort guess.
    #[test]
    fn unpinned_tools_do_not_resolve() {
        let p = pins();
        assert!(p
            .resolve(ToolKind::Pixi, "python", Some("3.12.*"))
            .is_none());
        assert!(p.resolve(ToolKind::Nvm, "node", Some("20")).is_none());
        assert!(p.resolve(ToolKind::Cargo, "rust", Some("1.79.0")).is_none());
        assert!(p.resolve(ToolKind::Apk, "git", None).is_none());
    }

    /// Error-path goldens: one assertion per semantic rule.
    #[test]
    fn error_paths_are_typed_and_path_scoped() {
        let base = r#"version = 1
[[pins]]
kind = "pixi"
name = "pixi"
version_spec = "0.34.*"
version = "0.34.0"
[[pins.artifacts]]
role = "primary"
version = "0.34.0"
arch = "x86_64"
format = "tar.gz"
file = "pixi.tar.gz"
url = "https://example.invalid/pixi.tar.gz"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
        assert!(ToolchainPins::from_toml(base).is_ok());

        let err = |text: &str| ToolchainPins::from_toml(text).expect_err("must be refused");

        // Schema version.
        let e = err(&base.replace("version = 1", "version = 2"));
        assert!(matches!(e, PinError::UnsupportedSchema { value: 2, .. }));
        assert_eq!(e.path(), Some("version"));

        // Empty table.
        assert!(matches!(
            err("version = 1\npins = []\n"),
            PinError::EmptyTable
        ));

        // Unknown field.
        assert!(matches!(
            err(&base.replace("role = \"primary\"", "role = \"primary\"\nnope = 1")),
            PinError::Toml { .. }
        ));

        // Duplicate pin: the same (kind, name, version_spec) declared twice.
        let entry = base.strip_prefix("version = 1\n").unwrap();
        let e = err(&format!("version = 1\n{entry}{entry}"));
        assert!(matches!(e, PinError::DuplicatePin { .. }), "{e}");
        assert_eq!(e.path(), Some("pins[1]"));

        // Pin without artifacts.
        let no_arts = base.split("[[pins.artifacts]]").next().unwrap();
        assert!(matches!(err(no_arts), PinError::NoArtifacts { .. }));

        // Bad digest (uppercase / short).
        let e = err(&base.replace(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ));
        assert!(
            matches!(e, PinError::BadValue { .. })
                && e.path() == Some("pins[0].artifacts[0].sha256"),
            "{e}"
        );
        let e = err(&base.replace(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "deadbeef",
        ));
        assert!(matches!(e, PinError::BadValue { .. }), "{e}");

        // Non-https URL is its own variant (ADR-0013 cl. 2: the first
        // download is the only unverified byte source).
        let e = err(&base.replace("https://example.invalid", "http://example.invalid"));
        assert!(matches!(e, PinError::InsecureUrl { .. }), "{e}");
        assert_eq!(e.path(), Some("pins[0].artifacts[0].url"));

        // Path-shaped or flag-shaped file names are refused (they land in argv).
        for bad in ["../evil.tar.gz", "-rf.tar.gz", "a/b.tar.gz", ""] {
            let e = err(&base.replace("pixi.tar.gz\"", &format!("{bad}\"")));
            assert!(
                matches!(e, PinError::BadValue { .. }),
                "file {bad:?} must be refused: {e}"
            );
            assert_eq!(e.path(), Some("pins[0].artifacts[0].file"));
        }

        // Duplicate artifact role.
        let e = err(&base.replace(
            "[[pins.artifacts]]\nrole = \"primary\"",
            "[[pins.artifacts]]\nrole = \"primary\"\nversion = \"0.34.0\"\narch = \"x86_64\"\nformat = \"binary\"\nfile = \"pixi\"\nurl = \"https://example.invalid/pixi\"\nsha256 = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"\n[[pins.artifacts]]\nrole = \"primary\"",
        ));
        assert!(matches!(e, PinError::DuplicateRole { .. }), "{e}");

        // Unknown format.
        assert!(matches!(
            err(&base.replace("format = \"tar.gz\"", "format = \"zip\"")),
            PinError::Toml { .. }
        ));
    }

    /// `tar_flag` is the extraction-argv selector the daemon renders.
    #[test]
    fn format_selects_the_tar_flag() {
        assert_eq!(ArtifactFormat::TarGz.tar_flag(), Some("-z"));
        assert_eq!(ArtifactFormat::TarXz.tar_flag(), Some("-J"));
        assert_eq!(ArtifactFormat::Binary.tar_flag(), None);
        assert_eq!(ArtifactFormat::TarXz.to_string(), "tar.xz");
    }

    /// Iteration order is sorted by `(kind, name, version_spec)`
    /// (CONVENTIONS §3 r1) — the bootstrap plan and every golden derived
    /// from it are deterministic regardless of file order.
    #[test]
    fn tools_iterate_in_sorted_key_order() {
        let table = pins();
        let keys: Vec<(ToolKind, &str, &str)> = table
            .tools()
            .map(|p| (p.kind(), p.name(), p.version_spec()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "BTreeMap iteration is sorted");
        assert_eq!(
            keys.iter().map(|(k, ..)| *k).collect::<Vec<_>>(),
            // ToolKind derives Ord in declaration order: Pixi, Nvm, Cargo, Apk.
            vec![ToolKind::Pixi, ToolKind::Nvm, ToolKind::Cargo]
        );
    }
}
