//! Path/name sanitizer (SPEC §11.3; rules locked by CONVENTIONS §4, ADR-0002).
//!
//! Regex-based allowlist `[A-Za-z0-9._-]`, a single internal `.` permitted as
//! a file-extension separator, length 1–64; rejects `.`, `..`, leading `-`,
//! control chars, path separators, and non-allowlisted unicode. Every name
//! derived from agent, Mayor, or webhook input passes through
//! [`sanitize_name`] before touching disk.
//!
//! [`SafeName`] is the only input accepted by `crate::tree` accessors that
//! take externally-sourced names (patron names, vault artifact names, mail
//! recipients) — traversal is blocked at the API boundary by construction,
//! not by convention. Golden test vectors are committed at the bottom of
//! this module.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

/// The §4 shape: one or more allowlisted chars (no leading `-`), optionally
/// followed by exactly one `.`-separated extension segment. This single
/// regex rejects `.`, `..`, leading/trailing dots, multi-dot names, leading
/// dashes, separators, and everything outside `[A-Za-z0-9._-]`.
static SHAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)?$").expect("static regex"));

/// Maximum name length in chars (CONVENTIONS §4: 1–64).
pub const MAX_NAME_LEN: usize = 64;

/// Why a candidate name was rejected. Check order (first failure wins) is
/// part of the golden-vector contract:
/// Empty → TooLong → PathSeparator → ControlChar → NonAscii → LeadingDash →
/// DotName → InvalidShape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizeError {
    /// Empty string.
    Empty,
    /// Longer than [`MAX_NAME_LEN`] chars.
    TooLong { len: usize },
    /// Contains `/` or `\`.
    PathSeparator,
    /// Contains an ASCII control char (< 0x20 or 0x7F), including NUL.
    ControlChar,
    /// Contains a non-ASCII char (allowlist is ASCII-only).
    NonAscii,
    /// Starts with `-` (would be read as a CLI flag downstream).
    LeadingDash,
    /// Literal `.` or `..` — traversal names are always rejected.
    DotName,
    /// Shape violation: leading/trailing dot, multiple dots, or a char
    /// outside the `[A-Za-z0-9._-]` allowlist.
    InvalidShape,
}

impl fmt::Display for SanitizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "name is empty"),
            Self::TooLong { len } => write!(f, "name exceeds {MAX_NAME_LEN} chars (got {len})"),
            Self::PathSeparator => write!(f, "name contains a path separator"),
            Self::ControlChar => write!(f, "name contains a control character"),
            Self::NonAscii => write!(f, "name contains non-ASCII characters"),
            Self::LeadingDash => write!(f, "name starts with '-'"),
            Self::DotName => write!(f, "name is '.' or '..'"),
            Self::InvalidShape => {
                write!(f, "name violates the allowlist shape [A-Za-z0-9_-]+(.ext)?")
            }
        }
    }
}

impl std::error::Error for SanitizeError {}

/// A validated name: guaranteed to match the CONVENTIONS §4 allowlist, to be
/// 1–64 chars, and to be free of traversal, separators, and control chars.
/// Constructible only via [`sanitize_name`] (or validated deserialization).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SafeName(String);

impl SafeName {
    /// The validated name as a string slice — safe as a single path segment.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for SafeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SafeName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validate `candidate` against CONVENTIONS §4 (ADR-0002).
///
/// The only public constructor of [`SafeName`]; every promotion, mail route,
/// decree target, and webhook filename uses it at the trust boundary.
///
/// # Errors
///
/// Returns the first [`SanitizeError`] in the documented check order.
pub fn sanitize_name(candidate: &str) -> Result<SafeName, SanitizeError> {
    if candidate.is_empty() {
        return Err(SanitizeError::Empty);
    }
    let len = candidate.chars().count();
    if len > MAX_NAME_LEN {
        return Err(SanitizeError::TooLong { len });
    }
    if candidate.contains(['/', '\\']) {
        return Err(SanitizeError::PathSeparator);
    }
    if candidate.chars().any(char::is_control) {
        return Err(SanitizeError::ControlChar);
    }
    if !candidate.is_ascii() {
        return Err(SanitizeError::NonAscii);
    }
    if candidate.starts_with('-') {
        return Err(SanitizeError::LeadingDash);
    }
    if candidate == "." || candidate == ".." {
        return Err(SanitizeError::DotName);
    }
    if !SHAPE.is_match(candidate) {
        return Err(SanitizeError::InvalidShape);
    }
    Ok(SafeName(candidate.to_owned()))
}

// --- serde: SafeName round-trips as a string, but deserialization re-validates
// so an untrusted charter/webhook payload can never inject an unsafe name. ---

impl serde::Serialize for SafeName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for SafeName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = SafeName;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    "a CONVENTIONS §4 sanitized name (1-64 chars, [A-Za-z0-9._-])"
                )
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<SafeName, E> {
                sanitize_name(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden ACCEPT vectors (CONVENTIONS §9: committed golden vectors).
    const ACCEPT: &[&str] = &[
        "a",
        "A",
        "0",
        "9lives",
        "agent-01",
        "agent_01",
        "mayor",
        "PROPRIETOR.mayor",
        "SKILL.md",
        "blueprint.toml",
        "COMMISSION",
        "ledger-2026_01",
        "x.y",
        "a-b_c.d",
        "underscore_name",
        "CamelCaseName",
        "0000",
        "f.ext",
        // exactly 64 chars — upper bound is inclusive
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ];

    /// Golden REJECT vectors with the exact first-failure error.
    const REJECT: &[(&str, SanitizeError)] = &[
        ("", SanitizeError::Empty),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            SanitizeError::TooLong { len: 65 },
        ),
        ("../etc/passwd", SanitizeError::PathSeparator),
        ("a/b", SanitizeError::PathSeparator),
        ("a\\b", SanitizeError::PathSeparator),
        ("/abs", SanitizeError::PathSeparator),
        ("nul\0inside", SanitizeError::ControlChar),
        ("\0", SanitizeError::ControlChar),
        ("tab\tname", SanitizeError::ControlChar),
        ("new\nline", SanitizeError::ControlChar),
        ("del\x7fchar", SanitizeError::ControlChar),
        ("café", SanitizeError::NonAscii),
        ("日本語", SanitizeError::NonAscii),
        ("emoji🎭", SanitizeError::NonAscii),
        ("naïve-agent", SanitizeError::NonAscii),
        ("-leading-dash", SanitizeError::LeadingDash),
        ("-", SanitizeError::LeadingDash),
        (".", SanitizeError::DotName),
        ("..", SanitizeError::DotName),
        (".hidden", SanitizeError::InvalidShape),
        ("trailing.", SanitizeError::InvalidShape),
        ("multi.dot.name", SanitizeError::InvalidShape),
        ("v1-rc_1.tar.gz", SanitizeError::InvalidShape),
        ("sp ace", SanitizeError::InvalidShape),
        ("semi;colon", SanitizeError::InvalidShape),
        ("quote\"name", SanitizeError::InvalidShape),
        ("back`tick", SanitizeError::InvalidShape),
        ("plus+one", SanitizeError::InvalidShape),
        ("at@sign", SanitizeError::InvalidShape),
        ("dollar$bill", SanitizeError::InvalidShape),
        ("per%cent", SanitizeError::InvalidShape),
        ("star*glob", SanitizeError::InvalidShape),
        ("colon:name", SanitizeError::InvalidShape),
        ("pipe|name", SanitizeError::InvalidShape),
        ("question?mark", SanitizeError::InvalidShape),
    ];

    #[test]
    fn golden_accept_vectors() {
        for name in ACCEPT {
            let ok = sanitize_name(name);
            assert!(ok.is_ok(), "expected ACCEPT for {name:?}, got {ok:?}");
            assert_eq!(ok.unwrap().as_str(), *name);
        }
    }

    #[test]
    fn golden_reject_vectors() {
        for (name, expected) in REJECT {
            assert_eq!(
                sanitize_name(name).unwrap_err(),
                *expected,
                "wrong error for {name:?}"
            );
        }
    }

    #[test]
    fn vector_counts_meet_minimum() {
        assert!(ACCEPT.len() + REJECT.len() >= 30);
        assert!(REJECT.len() >= 20);
    }

    #[test]
    fn safe_name_is_single_path_segment() {
        // No accepted name can alter a joined path's structure.
        for name in ACCEPT {
            let safe = sanitize_name(name).unwrap();
            let joined = std::path::Path::new("/village/keep/vault").join(safe.as_str());
            assert_eq!(
                joined.parent(),
                Some(std::path::Path::new("/village/keep/vault"))
            );
        }
    }

    #[test]
    fn serde_roundtrip_validates_on_deserialize() {
        let json = serde_json::to_string(&sanitize_name("SKILL.md").unwrap()).unwrap();
        assert_eq!(json, "\"SKILL.md\"");
        let back: SafeName = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "SKILL.md");
        assert!(serde_json::from_str::<SafeName>("\"../evil\"").is_err());
        assert!(serde_json::from_str::<SafeName>("\"\"").is_err());
    }
}
