//! External webhook dispatch protocol (SPEC §8.4; ADR-0020).
//!
//! Typed outbox request / inbox response file schemas for
//! `courier/dispatch/`, the URL allowlist matcher, the deterministic
//! (jitter-free) retry schedule, and the correlation ↔ filename mapping.
//! All pure: the daemon (`hamlet-daemon::dispatch`) owns reqwest, the
//! `.env` secret resolution, and every file write.
//!
//! # Secret hygiene (ADR-0020 cl. 1, 6; CONVENTIONS §6)
//!
//! The schema **cannot express an inline credential**: `headers_env` maps
//! header names to `.env` key names; values are resolved at send time only
//! and never written back into any tree artifact. Inbox header storage
//! redacts the sensitive set ([`REDACTED_HEADERS`]).
//!
//! # Golden vectors
//!
//! The exact canonical JSON bytes of one outbox request and one inbox
//! response are committed in the tests at the bottom of this module.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::session::CorrelationId;

/// Schema version stamped into every dispatch file (ADR-0020 cl. 1).
pub const DISPATCH_VERSION: u32 = 1;
/// Maximum outbox file size, bytes (ADR-0020 cl. 1).
pub const MAX_OUTBOX_BYTES: usize = 1024 * 1024;
/// Maximum URL length, chars (ADR-0020 cl. 1).
pub const MAX_URL_LEN: usize = 2048;
/// Maximum stored inbox body, bytes (ADR-0020 cl. 2).
pub const MAX_INBOX_BODY_BYTES: usize = 64 * 1024;
/// Maximum stored inbox header entries (ADR-0020 cl. 2).
pub const MAX_INBOX_HEADERS: usize = 32;
/// Maximum stored inbox header key length, bytes.
pub const MAX_INBOX_HEADER_KEY: usize = 128;
/// Maximum stored inbox header value length, bytes.
pub const MAX_INBOX_HEADER_VALUE: usize = 512;
/// Maximum inbox error string length, chars (ADR-0020 cl. 2).
pub const MAX_ERROR_LEN: usize = 128;
/// Headers never stored in inbox responses (ADR-0020 cl. 2).
pub const REDACTED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
];
/// Truncation marker appended to over-cap inbox bodies.
pub const TRUNCATION_MARKER: &str = "\n...[truncated]";
/// Dead-letter subdir name under `courier/dispatch/` (ADR-0020 cl. 5).
pub const DEAD_DIR: &str = "dead";

/// Why a dispatch artifact or policy input was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// Outbox file exceeds [`MAX_OUTBOX_BYTES`].
    Oversize { bytes: usize },
    /// `dispatch_version` is not [`DISPATCH_VERSION`].
    UnsupportedVersion(u32),
    /// JSON does not match the schema (message from serde).
    Schema(String),
    /// URL violates ADR-0020 cl. 1 (https-only, charset, no credentials).
    InvalidUrl(String),
    /// Header name is not a valid HTTP token.
    InvalidHeaderName(String),
    /// Env key name violates `[A-Z][A-Z0-9_]{0,63}`.
    InvalidEnvKey(String),
    /// Allowlist entry violates ADR-0020 cl. 3.
    InvalidAllowlistEntry(String),
    /// Retry policy values out of range.
    InvalidRetryPolicy(String),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { bytes } => {
                write!(
                    f,
                    "outbox file exceeds {MAX_OUTBOX_BYTES} bytes (got {bytes})"
                )
            }
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported dispatch_version {v} (this build writes/accepts {DISPATCH_VERSION})")
            }
            Self::Schema(msg) => write!(f, "dispatch schema violation: {msg}"),
            Self::InvalidUrl(url) => write!(f, "invalid dispatch URL: {url}"),
            Self::InvalidHeaderName(name) => write!(f, "invalid HTTP header name: {name}"),
            Self::InvalidEnvKey(key) => write!(f, "invalid .env key reference: {key}"),
            Self::InvalidAllowlistEntry(entry) => write!(f, "invalid URL allowlist entry: {entry}"),
            Self::InvalidRetryPolicy(msg) => write!(f, "invalid retry policy: {msg}"),
        }
    }
}

impl std::error::Error for DispatchError {}

/// HTTP method of an outbox request (ADR-0020 cl. 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        };
        f.write_str(name)
    }
}

fn header_token_re() -> &'static Regex {
    // RFC 9110 token charset.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9!#$%&'*+\-.^_`|~]+$").expect("static regex"))
}

fn env_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Z][A-Z0-9_]{0,63}$").expect("static regex"))
}

/// Does `key` match the `.env` key-reference grammar `[A-Z][A-Z0-9_]{0,63}`
/// (ADR-0020 cl. 1)? Shared by outbox `headers_env` validation and the
/// config surface (`webhooks.auth_env_key`, ADR-0022).
#[must_use]
pub fn is_env_key(key: &str) -> bool {
    env_key_re().is_match(key)
}

/// RFC 3986 charset (plus `%` for percent-encoding); no whitespace/control.
fn url_charset_ok(url: &str) -> bool {
    url.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | '~'
                    | ':'
                    | '/'
                    | '?'
                    | '#'
                    | '['
                    | ']'
                    | '@'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
            )
    })
}

/// Validate an absolute `https://` URL (ADR-0020 cl. 1): scheme, charset,
/// no userinfo credentials, non-empty host, bounded length.
///
/// # Errors
///
/// [`DispatchError::InvalidUrl`] on any violation.
pub fn validate_url(url: &str) -> Result<(), DispatchError> {
    let invalid = || DispatchError::InvalidUrl(url.to_owned());
    if url.len() > MAX_URL_LEN || !url.starts_with("https://") {
        return Err(invalid());
    }
    let rest = &url["https://".len()..];
    if !url_charset_ok(rest) {
        return Err(invalid());
    }
    let (authority, _) = rest.split_once(['/', '?', '#']).unwrap_or((rest, ""));
    // Non-empty authority, no userinfo credentials, no bare port.
    if authority.is_empty() || authority.contains('@') || authority.ends_with(':') {
        return Err(invalid());
    }
    let host = authority.split_once(':').map_or(authority, |(h, port)| {
        if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            ""
        }
    });
    if host.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

/// Split `https://authority/path…` into (authority, path-with-query).
/// Panics are impossible: callers validate first; fallible form returns
/// `None` for non-https input.
fn split_authority(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (authority, tail) = rest.split_once(['/', '?', '#']).unwrap_or((rest, ""));
    Some((authority, tail))
}

/// One validated allowlist entry: `https://host[:port][/path-prefix]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowlistEntry {
    /// Lowercased `host[:port]` authority.
    pub authority: String,
    /// Byte-exact path prefix (empty = whole origin; starts with `/` else).
    pub path_prefix: String,
}

/// URL allowlist policy from `config.json` `dispatch.url_allowlist`
/// (ADR-0020 cl. 3). An empty list denies everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UrlAllowlist {
    entries: Vec<AllowlistEntry>,
}

impl UrlAllowlist {
    /// Validate config entries.
    ///
    /// # Errors
    ///
    /// [`DispatchError::InvalidAllowlistEntry`] for any malformed entry.
    pub fn parse(entries: &[String]) -> Result<Self, DispatchError> {
        let mut parsed = Vec::with_capacity(entries.len());
        for entry in entries {
            validate_url(entry).map_err(|_| DispatchError::InvalidAllowlistEntry(entry.clone()))?;
            let (authority, path) = split_authority(entry)
                .ok_or_else(|| DispatchError::InvalidAllowlistEntry(entry.clone()))?;
            parsed.push(AllowlistEntry {
                authority: authority.to_ascii_lowercase(),
                path_prefix: path.to_owned(),
            });
        }
        Ok(Self { entries: parsed })
    }

    /// Number of entries (0 = deny-all).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the list is empty (deny-all posture).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Does `url` match some entry? Match = exact authority
    /// (case-insensitive host, exact port) + byte-prefix path.
    #[must_use]
    pub fn allows(&self, url: &str) -> bool {
        let Some((authority, path)) = split_authority(url) else {
            return false;
        };
        let authority = authority.to_ascii_lowercase();
        self.entries.iter().any(|entry| {
            // An entry authority may omit the port; a URL authority may too.
            // Exactness means: both sides' authority strings must be equal.
            entry.authority == authority && path.starts_with(&entry.path_prefix)
        })
    }
}

/// Outbox request file: `courier/dispatch/outbox/<correlation>.json`
/// (ADR-0020 cl. 1). Field declaration order is alphabetical — serde emits
/// keys in order, so `render` output is canonical (CONVENTIONS §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboxRequest {
    /// Arbitrary JSON payload (may be null).
    pub body: serde_json::Value,
    /// End-to-end correlation; dispatch canonical form `d-<seq>-<uid>.<inc>`.
    pub correlation: CorrelationId,
    /// Must equal [`DISPATCH_VERSION`].
    pub dispatch_version: u32,
    /// Header name → `.env` key name (**never** an inline value).
    pub headers_env: BTreeMap<String, String>,
    /// HTTP method.
    pub method: Method,
    /// Target URL (https-only; checked again against the allowlist).
    pub url: String,
}

impl OutboxRequest {
    /// Validate beyond the serde schema: version, URL shape, header tokens,
    /// env-key grammar.
    ///
    /// # Errors
    ///
    /// The first [`DispatchError`] found.
    pub fn validate(&self) -> Result<(), DispatchError> {
        if self.dispatch_version != DISPATCH_VERSION {
            return Err(DispatchError::UnsupportedVersion(self.dispatch_version));
        }
        validate_url(&self.url)?;
        for (name, key) in &self.headers_env {
            if !header_token_re().is_match(name) {
                return Err(DispatchError::InvalidHeaderName(name.clone()));
            }
            if !env_key_re().is_match(key) {
                return Err(DispatchError::InvalidEnvKey(key.clone()));
            }
        }
        Ok(())
    }

    /// Parse an outbox file's contents (size-capped, schema, validate).
    ///
    /// # Errors
    ///
    /// [`DispatchError::Oversize`], [`DispatchError::Schema`], or whatever
    /// [`Self::validate`] reports.
    pub fn parse(contents: &str) -> Result<Self, DispatchError> {
        if contents.len() > MAX_OUTBOX_BYTES {
            return Err(DispatchError::Oversize {
                bytes: contents.len(),
            });
        }
        let request: Self =
            serde_json::from_str(contents).map_err(|e| DispatchError::Schema(e.to_string()))?;
        request.validate()?;
        Ok(request)
    }

    /// Canonical JSON rendering (pretty, 2-space, alphabetical keys).
    ///
    /// # Errors
    ///
    /// Propagates serde errors (cannot fail for validated values).
    pub fn render(&self) -> Result<String, DispatchError> {
        serde_json::to_string_pretty(self).map_err(|e| DispatchError::Schema(e.to_string()))
    }
}

/// Inbox outcome (ADR-0020 cl. 2), internally tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// A delivered HTTP response (any status — 4xx included).
    Http {
        /// HTTP status code.
        status: u16,
        /// Sanitized lowercased header subset (redacted + capped).
        headers: BTreeMap<String, String>,
        /// Body text, capped at [`MAX_INBOX_BODY_BYTES`].
        body: String,
    },
    /// No HTTP response obtained (transport error, validation rejection,
    /// retry exhaustion). `error` is opaque ASCII ≤ [`MAX_ERROR_LEN`].
    Error {
        /// Error classification token/message.
        error: String,
    },
}

/// Inbox response file: `courier/dispatch/inbox/<correlation>.json`
/// (ADR-0020 cl. 2). Field order alphabetical (canonical render).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxResponse {
    /// 1-based attempt counter of the attempt this response records.
    pub attempt: u32,
    /// Echoed from the outbox request.
    pub correlation: CorrelationId,
    /// Must equal [`DISPATCH_VERSION`].
    pub dispatch_version: u32,
    /// The recorded outcome.
    pub outcome: Outcome,
}

impl InboxResponse {
    /// Build an HTTP-outcome response, applying the cl. 2 storage
    /// sanitization (lowercase, redact, caps, truncation).
    #[must_use]
    pub fn http(
        correlation: CorrelationId,
        attempt: u32,
        status: u16,
        headers: &BTreeMap<String, String>,
        body: &str,
    ) -> Self {
        let mut stored = BTreeMap::new();
        for (name, value) in headers {
            let key = name.to_ascii_lowercase();
            if REDACTED_HEADERS.contains(&key.as_str()) || stored.len() >= MAX_INBOX_HEADERS {
                continue;
            }
            if key.len() > MAX_INBOX_HEADER_KEY {
                continue;
            }
            let mut value = value.clone();
            if value.len() > MAX_INBOX_HEADER_VALUE {
                value.truncate(MAX_INBOX_HEADER_VALUE);
            }
            stored.insert(key, value);
        }
        Self {
            attempt,
            correlation,
            dispatch_version: DISPATCH_VERSION,
            outcome: Outcome::Http {
                status,
                headers: stored,
                body: truncate_body(body),
            },
        }
    }

    /// Build an error-outcome response (message clipped to
    /// [`MAX_ERROR_LEN`] and stripped to printable ASCII).
    #[must_use]
    pub fn error(correlation: CorrelationId, attempt: u32, error: &str) -> Self {
        let clipped: String = error
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c
                } else {
                    '?'
                }
            })
            .take(MAX_ERROR_LEN)
            .collect();
        Self {
            attempt,
            correlation,
            dispatch_version: DISPATCH_VERSION,
            outcome: Outcome::Error { error: clipped },
        }
    }

    /// Parse an inbox file's contents.
    ///
    /// # Errors
    ///
    /// [`DispatchError::Schema`] or [`DispatchError::UnsupportedVersion`].
    pub fn parse(contents: &str) -> Result<Self, DispatchError> {
        let response: Self =
            serde_json::from_str(contents).map_err(|e| DispatchError::Schema(e.to_string()))?;
        if response.dispatch_version != DISPATCH_VERSION {
            return Err(DispatchError::UnsupportedVersion(response.dispatch_version));
        }
        Ok(response)
    }

    /// Canonical JSON rendering (pretty, 2-space, alphabetical keys).
    ///
    /// # Errors
    ///
    /// Propagates serde errors (cannot fail for validated values).
    pub fn render(&self) -> Result<String, DispatchError> {
        serde_json::to_string_pretty(self).map_err(|e| DispatchError::Schema(e.to_string()))
    }
}

fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_INBOX_BODY_BYTES {
        return body.to_owned();
    }
    let mut cut = MAX_INBOX_BODY_BYTES.saturating_sub(TRUNCATION_MARKER.len());
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &body[..cut])
}

/// Is an HTTP status retryable (ADR-0020 cl. 4)? 429 and 5xx retry;
/// every other status is a permanent delivered response.
#[must_use]
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || status >= 500
}

/// Deterministic bounded-exponential retry policy (ADR-0020 cl. 4).
/// Jitter-free: the same inputs always yield the same schedule
/// (CONVENTIONS §3 — reproducible state derivation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts before dead-letter (≥1).
    pub max_attempts: u32,
    /// Delay before attempt 2, ms.
    pub initial_ms: u64,
    /// Delay cap, ms.
    pub max_ms: u64,
}

impl RetryPolicy {
    /// Validate the policy bounds (mirrors the `config.json` v5 ranges).
    ///
    /// # Errors
    ///
    /// [`DispatchError::InvalidRetryPolicy`] when out of range.
    pub fn validate(&self) -> Result<(), DispatchError> {
        let invalid = |msg: &str| DispatchError::InvalidRetryPolicy(msg.to_owned());
        if self.max_attempts == 0 || self.max_attempts > 16 {
            return Err(invalid("max_attempts must be in 1..=16"));
        }
        if self.initial_ms < 100 || self.initial_ms > 3_600_000 {
            return Err(invalid("initial_ms must be in 100..=3600000"));
        }
        if self.max_ms < self.initial_ms || self.max_ms > 86_400_000 {
            return Err(invalid("max_ms must be in initial_ms..=86400000"));
        }
        Ok(())
    }

    /// Delay in ms before attempt `next_attempt` (1-based), or `None` when
    /// the budget is exhausted (the failed attempt goes to dead-letter).
    /// `delay(n) = min(initial_ms · 2^(n−2), max_ms)` for n ≥ 2.
    #[must_use]
    pub fn delay_before(&self, next_attempt: u32) -> Option<u64> {
        if next_attempt < 2 || next_attempt > self.max_attempts {
            return None;
        }
        let exp = next_attempt.saturating_sub(2);
        let delay = self.initial_ms.saturating_mul(1_u64 << exp.min(63));
        Some(delay.min(self.max_ms))
    }

    /// The full delay schedule (attempts 2..=max), for tests/QA.
    #[must_use]
    pub fn schedule(&self) -> Vec<u64> {
        (2..=self.max_attempts)
            .filter_map(|n| self.delay_before(n))
            .collect()
    }
}

/// Dispatch file name for a correlation id: `<correlation>.json`.
#[must_use]
pub fn file_name(correlation: &CorrelationId) -> String {
    format!("{}.json", correlation)
}

/// Extract the correlation id from a `<correlation>.json` file name.
///
/// # Errors
///
/// Propagates [`CorrelationId::parse`] (charset/length) via
/// [`DispatchError::Schema`]; `None`-style callers use `.ok()`.
pub fn correlation_from_file_name(name: &str) -> Result<CorrelationId, DispatchError> {
    let stem = name
        .strip_suffix(".json")
        .ok_or_else(|| DispatchError::Schema(format!("{name} is not a .json dispatch file")))?;
    CorrelationId::parse(stem).map_err(|e| DispatchError::Schema(e.to_string()))
}

// ---------------------------------------------------------------------------
// Golden vectors (CONVENTIONS §9 — committed contract)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn corr(raw: &str) -> CorrelationId {
        CorrelationId::parse(raw).expect("valid correlation")
    }

    /// Committed golden vector: exact canonical bytes of an outbox request.
    #[test]
    fn outbox_render_golden() {
        let request = OutboxRequest {
            body: serde_json::json!({"event": "deed-updated", "n": 7}),
            correlation: corr("d-3-1000.1"),
            dispatch_version: DISPATCH_VERSION,
            headers_env: BTreeMap::from([(
                "X-Hamlet-Auth".to_owned(),
                "HAMLET_DISPATCH_AUTH".to_owned(),
            )]),
            method: Method::Post,
            url: "https://hooks.example.invalid/v1/hamlet".to_owned(),
        };
        let expected = "\
{
  \"body\": {
    \"event\": \"deed-updated\",
    \"n\": 7
  },
  \"correlation\": \"d-3-1000.1\",
  \"dispatch_version\": 1,
  \"headers_env\": {
    \"X-Hamlet-Auth\": \"HAMLET_DISPATCH_AUTH\"
  },
  \"method\": \"post\",
  \"url\": \"https://hooks.example.invalid/v1/hamlet\"
}";
        assert_eq!(request.render().unwrap(), expected);
        assert_eq!(OutboxRequest::parse(expected).unwrap(), request);
    }

    /// Committed golden vector: exact canonical bytes of an inbox response.
    #[test]
    fn inbox_render_golden() {
        let response = InboxResponse::http(
            corr("d-3-1000.1"),
            2,
            200,
            &BTreeMap::from([
                ("Content-Type".to_owned(), "application/json".to_owned()),
                (
                    "Authorization".to_owned(),
                    "Bearer test-key-do-not-use".to_owned(),
                ),
            ]),
            "{\"ok\":true}",
        );
        let expected = "\
{
  \"attempt\": 2,
  \"correlation\": \"d-3-1000.1\",
  \"dispatch_version\": 1,
  \"outcome\": {
    \"kind\": \"http\",
    \"status\": 200,
    \"headers\": {
      \"content-type\": \"application/json\"
    },
    \"body\": \"{\\\"ok\\\":true}\"
  }
}";
        assert_eq!(response.render().unwrap(), expected);
        let parsed = InboxResponse::parse(expected).unwrap();
        assert_eq!(parsed, response);
        // Authorization was redacted on construction.
        match &parsed.outcome {
            Outcome::Http { headers, .. } => assert!(!headers.contains_key("authorization")),
            Outcome::Error { .. } => panic!("expected http outcome"),
        }
    }

    /// Golden vector: error outcome + clipping.
    #[test]
    fn inbox_error_golden() {
        let response = InboxResponse::error(corr("d-4-1000.1"), 5, "timeout after 30s");
        let rendered = response.render().unwrap();
        assert!(rendered.contains("\"kind\": \"error\""));
        assert!(rendered.contains("\"error\": \"timeout after 30s\""));
        let long: String = "x".repeat(300);
        let clipped = InboxResponse::error(corr("d-4-1000.1"), 1, &long);
        match clipped.outcome {
            Outcome::Error { error } => assert_eq!(error.len(), MAX_ERROR_LEN),
            Outcome::Http { .. } => panic!("expected error outcome"),
        }
        // control chars are replaced, never passed through
        let dirty = InboxResponse::error(corr("d-4-1000.1"), 1, "bad\x00\nvalue");
        match dirty.outcome {
            Outcome::Error { error } => assert_eq!(error, "bad??value"),
            Outcome::Http { .. } => panic!("expected error outcome"),
        }
    }

    /// Golden vector: the default retry schedule is deterministic and
    /// bounded — 5s, 10s, 20s, 40s (ADR-0020 cl. 4 defaults).
    #[test]
    fn retry_schedule_golden() {
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_ms: 5_000,
            max_ms: 900_000,
        };
        policy.validate().unwrap();
        assert_eq!(policy.schedule(), vec![5_000, 10_000, 20_000, 40_000]);
        assert_eq!(policy.delay_before(1), None);
        assert_eq!(policy.delay_before(6), None);
        // cap engagement
        let capped = RetryPolicy {
            max_attempts: 8,
            initial_ms: 60_000,
            max_ms: 120_000,
        };
        assert_eq!(
            capped.schedule(),
            vec![60_000, 120_000, 120_000, 120_000, 120_000, 120_000, 120_000]
        );
        // overflow-safe at the extreme
        let huge = RetryPolicy {
            max_attempts: 16,
            initial_ms: 3_600_000,
            max_ms: 86_400_000,
        };
        assert!(huge.validate().is_ok());
        assert_eq!(huge.delay_before(16), Some(86_400_000));
        assert!(RetryPolicy {
            max_attempts: 0,
            initial_ms: 100,
            max_ms: 100
        }
        .validate()
        .is_err());
        assert!(RetryPolicy {
            max_attempts: 3,
            initial_ms: 10,
            max_ms: 100
        }
        .validate()
        .is_err());
        assert!(RetryPolicy {
            max_attempts: 3,
            initial_ms: 200,
            max_ms: 100
        }
        .validate()
        .is_err());
    }

    #[test]
    fn retryable_status_table() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(499));
    }

    /// Allowlist: exact authority + path-prefix matching; deny-all default.
    #[test]
    fn allowlist_matching() {
        let list = UrlAllowlist::parse(&[
            "https://hooks.example.invalid/v1/hamlet".to_owned(),
            "https://api.example.invalid:8443".to_owned(),
        ])
        .unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.allows("https://hooks.example.invalid/v1/hamlet"));
        assert!(list.allows("https://hooks.example.invalid/v1/hamlet/deep?x=1"));
        assert!(!list.allows("https://hooks.example.invalid/v1/other"));
        assert!(!list.allows("https://HOOKS.example.invalid:443/v1/hamlet"));
        assert!(list.allows("https://api.example.invalid:8443/anything"));
        assert!(!list.allows("https://api.example.invalid/anything"));
        assert!(!list.allows("http://hooks.example.invalid/v1/hamlet"));
        assert!(!list.allows("https://evil.invalid/v1/hamlet"));
        // empty list denies everything
        assert!(!UrlAllowlist::default().allows("https://hooks.example.invalid/v1/hamlet"));
        assert!(UrlAllowlist::default().is_empty());
        // entries must themselves be valid https URLs
        assert!(UrlAllowlist::parse(&["http://x.invalid".to_owned()]).is_err());
        assert!(UrlAllowlist::parse(&["ftp://x.invalid".to_owned()]).is_err());
    }

    #[test]
    fn url_validation() {
        assert!(validate_url("https://example.invalid/hook").is_ok());
        assert!(validate_url("https://example.invalid:8443/a?b=c#d").is_ok());
        assert!(validate_url("http://example.invalid").is_err());
        assert!(validate_url("https://example.invalid:notaport/x").is_err());
        assert!(validate_url("https://user:pass@example.invalid").is_err());
        assert!(validate_url("https://").is_err());
        assert!(validate_url("https://example.invalid/has space").is_err());
        assert!(validate_url(&format!("https://x.invalid/{}", "a".repeat(2100))).is_err());
    }

    #[test]
    fn outbox_rejections() {
        let base = OutboxRequest {
            body: serde_json::Value::Null,
            correlation: corr("d-1-1000.1"),
            dispatch_version: DISPATCH_VERSION,
            headers_env: BTreeMap::new(),
            method: Method::Post,
            url: "https://example.invalid/hook".to_owned(),
        };
        base.validate().unwrap();
        let bad_version = OutboxRequest {
            dispatch_version: 2,
            ..base.clone()
        };
        assert!(matches!(
            bad_version.validate(),
            Err(DispatchError::UnsupportedVersion(2))
        ));
        let bad_url = OutboxRequest {
            url: "http://example.invalid".to_owned(),
            ..base.clone()
        };
        assert!(matches!(
            bad_url.validate(),
            Err(DispatchError::InvalidUrl(_))
        ));
        let bad_header = OutboxRequest {
            headers_env: BTreeMap::from([("X Bad".to_owned(), "KEY".to_owned())]),
            ..base.clone()
        };
        assert!(matches!(
            bad_header.validate(),
            Err(DispatchError::InvalidHeaderName(_))
        ));
        let bad_key = OutboxRequest {
            headers_env: BTreeMap::from([("X-Auth".to_owned(), "lower_case".to_owned())]),
            ..base
        };
        assert!(matches!(
            bad_key.validate(),
            Err(DispatchError::InvalidEnvKey(_))
        ));
        // oversize + schema + unknown fields
        let huge = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_OUTBOX_BYTES));
        assert!(matches!(
            OutboxRequest::parse(&huge),
            Err(DispatchError::Oversize { .. })
        ));
        assert!(matches!(
            OutboxRequest::parse("{}"),
            Err(DispatchError::Schema(_))
        ));
        let unknown = "{\"body\":null,\"correlation\":\"d-1-1000.1\",\"dispatch_version\":1,\"headers_env\":{},\"method\":\"post\",\"url\":\"https://x.invalid\",\"secret\":\"inline\"}";
        assert!(matches!(
            OutboxRequest::parse(unknown),
            Err(DispatchError::Schema(_))
        ));
    }

    #[test]
    fn body_truncation_char_boundary_safe() {
        let body: String = "ü".repeat(MAX_INBOX_BODY_BYTES); // 2-byte chars
        let response = InboxResponse::http(corr("d-1-1000.1"), 1, 200, &BTreeMap::new(), &body);
        match response.outcome {
            Outcome::Http { body: stored, .. } => {
                assert!(stored.ends_with(TRUNCATION_MARKER));
                assert!(stored.len() <= MAX_INBOX_BODY_BYTES);
            }
            Outcome::Error { .. } => panic!("expected http outcome"),
        }
    }

    #[test]
    fn correlation_file_names() {
        let id = corr("d-12-1042.3");
        assert_eq!(file_name(&id), "d-12-1042.3.json");
        assert_eq!(correlation_from_file_name("d-12-1042.3.json").unwrap(), id);
        assert!(correlation_from_file_name("d-12-1042.3.txt").is_err());
        assert!(correlation_from_file_name("bad chars!.json").is_err());
        // Dispatch file names are safe single path segments: the correlation
        // charset `[A-Za-z0-9._-]` excludes separators and traversal, so no
        // segment can escape `courier/dispatch/`. They are NOT single-dot
        // `sanitize_name` names (`<uid>.<inc>.json` carries two dots) — the
        // daemon builds them only via `file_name`, never from raw input.
        let fname = file_name(&id);
        assert!(!fname.contains('/') && !fname.contains('\\'));
        assert_ne!(fname, ".");
        assert_ne!(fname, "..");
    }
}
