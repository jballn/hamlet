//! Mail format + spool routing types (SPEC §8.1; ADR-0019).
//!
//! Plain-text spool messages with a fixed four-header preamble
//! (`From:`/`To:`/`Subject:`/`Date:`), file naming under
//! `courier/mail/<recipient>/`, and pure routing support (bounce reasons,
//! spool-name grammar, visibility rules). Delivery side effects — the
//! inotify watcher, spool renames, queueing into the Sprint 03
//! `QueueStore` — belong to `hamlet-daemon::mailroute`, never here
//! (purity seam, CONVENTIONS §2).
//!
//! # Format contract (ADR-0019 cl. 1)
//!
//! ```text
//! From: 1042
//! To: 1001
//! Subject: Report ready
//! Date: 2025-06-01T12:00:00Z
//!
//! Body text (UTF-8, non-empty, ≤ 64 KiB).
//! ```
//!
//! - LF-only: **any** carriage return in the message is a parse rejection
//!   (header-injection defense).
//! - `Date:` is opaque lineage data — parsed as a token, never interpreted
//!   into state (CONVENTIONS §3.4).
//! - [`Mail::parse`] / [`Mail::render`] round-trip byte-exact; the golden
//!   vectors at the bottom of this module are the committed contract.
//!
//! # Spool contract (ADR-0019 cl. 2–4)
//!
//! - Incoming = non-hidden files ending in `.mail` inside
//!   [`crate::tree::HamletTree::courier_mail_recipient`].
//! - Writer grammar [`spool_file_name`]: `{seq:012}-{writer_uid}-{nonce}.mail`
//!   so per-writer FIFO equals lexicographic scan order; foreign `.mail`
//!   names remain valid mail (sorted lexicographically).
//! - Writers stage `.`-prefixed temp files and rename into place; the ack
//!   rename target is the hidden `.read/` subdir
//!   ([`crate::tree::HamletTree::courier_mail_read`]) — hidden paths are
//!   never incoming and never rescanned.

use std::fmt;

use crate::sanitize::{sanitize_name, SafeName};
use crate::slots::{FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};

/// Maximum `Subject:` length in chars (ADR-0019 cl. 1).
pub const MAX_SUBJECT_LEN: usize = 128;
/// Maximum `Date:` token length in chars (ADR-0019 cl. 1).
pub const MAX_DATE_LEN: usize = 64;
/// Maximum body size in bytes (ADR-0019 cl. 1).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Maximum whole-message size accepted by [`Mail::parse`], bytes.
pub const MAX_MESSAGE_BYTES: usize = MAX_BODY_BYTES + 4 * (MAX_SUBJECT_LEN + MAX_DATE_LEN + 64);
/// Spool file extension: only non-hidden `*.mail` files are incoming.
pub const SPOOL_EXT: &str = "mail";
/// Hidden ack subdirectory inside a recipient spool (ADR-0019 cl. 3).
pub const READ_DIR: &str = ".read";
/// Bounce mails are daemon-composed under this sender name.
pub const SYSTEM_SENDER: &str = "system";

/// Why a [`Mail`] failed validation or parsing. The variant order is the
/// parse check order (first failure wins) — part of the golden contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailError {
    /// Empty message.
    Empty,
    /// Whole message exceeds [`MAX_MESSAGE_BYTES`].
    MessageTooLarge { bytes: usize },
    /// A carriage return appears anywhere (LF-only contract).
    CarriageReturn,
    /// A required header line is missing or misordered at `index` (0-based).
    HeaderMissing { index: usize },
    /// The blank separator line between headers and body is missing.
    NoSeparator,
    /// `From:` value is neither a decimal slot UID nor a valid SafeName.
    InvalidFrom { value: String },
    /// `To:` value is not a decimal UID inside the session slot range.
    InvalidTo { value: String },
    /// `Subject:` empty, overlong, control chars, or edge whitespace.
    InvalidSubject { reason: &'static str },
    /// `Date:` empty, overlong, or control chars.
    InvalidDate { reason: &'static str },
    /// Body empty after the separator.
    EmptyBody,
    /// Body exceeds [`MAX_BODY_BYTES`].
    BodyTooLarge { bytes: usize },
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "mail message is empty"),
            Self::MessageTooLarge { bytes } => {
                write!(
                    f,
                    "mail message exceeds {MAX_MESSAGE_BYTES} bytes (got {bytes})"
                )
            }
            Self::CarriageReturn => write!(f, "mail message contains CR (LF-only contract)"),
            Self::HeaderMissing { index } => {
                let name = ["From:", "To:", "Subject:", "Date:"][*index.min(&3)];
                write!(f, "header line {index} must start with {name}")
            }
            Self::NoSeparator => write!(f, "blank line between headers and body is missing"),
            Self::InvalidFrom { value } => {
                write!(f, "From {value:?} is neither a slot UID nor a safe name")
            }
            Self::InvalidTo { value } => write!(f, "To {value:?} is not a decimal slot UID"),
            Self::InvalidSubject { reason } => write!(f, "invalid Subject: {reason}"),
            Self::InvalidDate { reason } => write!(f, "invalid Date: {reason}"),
            Self::EmptyBody => write!(f, "mail body must not be empty"),
            Self::BodyTooLarge { bytes } => {
                write!(f, "mail body exceeds {MAX_BODY_BYTES} bytes (got {bytes})")
            }
        }
    }
}

impl std::error::Error for MailError {}

/// True when `uid` is the Mayor slot or an allocatable agent slot.
#[must_use]
pub fn is_slot_uid(uid: u32) -> bool {
    uid == MAYOR_UID || (FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid)
}

/// The `From:` identity. UIDs are slot-owned identities; names are
/// non-slot senders (patrons, `system` bounces) — always [`SafeName`],
/// so a From value can never carry a traversal or a control char.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MailSender {
    /// A slot UID (Mayor 1000 or agent 1001–2000), rendered decimal.
    Uid(u32),
    /// A non-slot identity (e.g. patron name, `system`).
    Name(SafeName),
}

impl MailSender {
    /// Parse a `From:` value: all-decimal → slot UID (range-checked),
    /// otherwise → sanitized name.
    ///
    /// # Errors
    ///
    /// [`MailError::InvalidFrom`] when neither form validates.
    pub fn parse(value: &str) -> Result<Self, MailError> {
        let invalid = || MailError::InvalidFrom {
            value: value.to_owned(),
        };
        if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
            let uid: u32 = value.parse().map_err(|_| invalid())?;
            if is_slot_uid(uid) {
                return Ok(Self::Uid(uid));
            }
            return Err(invalid());
        }
        sanitize_name(value).map(Self::Name).map_err(|_| invalid())
    }
}

impl fmt::Display for MailSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uid(uid) => write!(f, "{uid}"),
            Self::Name(name) => f.write_str(name.as_str()),
        }
    }
}

/// The validated four-header preamble (ADR-0019 cl. 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailHeader {
    /// Sender identity. Daemon-asserted when the daemon writes; taken
    /// as-is (but charset-safe) when an agent wrote the spool file.
    pub from: MailSender,
    /// Recipient slot UID — always the spool owner for routed mail.
    pub to: u32,
    /// Single-line subject, 1–[`MAX_SUBJECT_LEN`] chars, no control chars.
    pub subject: String,
    /// Opaque lineage token (RFC 3339 recommended); never state input.
    pub date: String,
}

/// A complete spool message: validated header + body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    /// The preamble.
    pub header: MailHeader,
    /// UTF-8 body, non-empty, ≤ [`MAX_BODY_BYTES`].
    pub body: String,
}

impl Mail {
    /// Render the canonical wire form (LF-only, byte-exact round-trip with
    /// [`Mail::parse`]).
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "From: {}\nTo: {}\nSubject: {}\nDate: {}\n\n{}",
            self.header.from, self.header.to, self.header.subject, self.header.date, self.body
        )
    }

    /// Parse and validate a spool file's contents.
    ///
    /// # Errors
    ///
    /// The first [`MailError`] in documented check order.
    pub fn parse(message: &str) -> Result<Self, MailError> {
        if message.is_empty() {
            return Err(MailError::Empty);
        }
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(MailError::MessageTooLarge {
                bytes: message.len(),
            });
        }
        if message.contains('\r') {
            return Err(MailError::CarriageReturn);
        }
        let mut lines = message.split('\n');
        let prefixes = ["From: ", "To: ", "Subject: ", "Date: "];
        let mut values = Vec::with_capacity(4);
        for (index, prefix) in prefixes.iter().enumerate() {
            let line = lines
                .next()
                .filter(|l| l.starts_with(prefix))
                .ok_or(MailError::HeaderMissing { index })?;
            values.push(&line[prefix.len()..]);
        }
        // Separator: exactly one empty line after Date.
        if lines.next() != Some("") {
            return Err(MailError::NoSeparator);
        }
        let from = MailSender::parse(values[0])?;
        let to: u32 = values[1]
            .parse()
            .ok()
            .filter(|uid| is_slot_uid(*uid))
            .ok_or_else(|| MailError::InvalidTo {
                value: values[1].to_owned(),
            })?;
        let subject = validate_subject(values[2])?;
        let date = validate_date(values[3])?;
        let body = lines.collect::<Vec<_>>().join("\n");
        // Trailing newline tolerance: exactly one final LF is canonical.
        let body = body.strip_suffix('\n').unwrap_or(&body).to_owned();
        if body.is_empty() {
            return Err(MailError::EmptyBody);
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(MailError::BodyTooLarge { bytes: body.len() });
        }
        Ok(Self {
            header: MailHeader {
                from,
                to,
                subject,
                date,
            },
            body: body.to_owned(),
        })
    }
}

fn has_control(value: &str) -> bool {
    value.chars().any(|c| c.is_control())
}

fn validate_subject(value: &str) -> Result<String, MailError> {
    let invalid = |reason| Err(MailError::InvalidSubject { reason });
    if value.is_empty() {
        return invalid("must not be empty");
    }
    if value.chars().count() > MAX_SUBJECT_LEN {
        return invalid("exceeds 128 chars");
    }
    if has_control(value) {
        return invalid("contains control characters");
    }
    if value.starts_with(' ') || value.ends_with(' ') {
        return invalid("must not start or end with a space");
    }
    Ok(value.to_owned())
}

fn validate_date(value: &str) -> Result<String, MailError> {
    let invalid = |reason| Err(MailError::InvalidDate { reason });
    if value.is_empty() {
        return invalid("must not be empty (lineage token)");
    }
    if value.chars().count() > MAX_DATE_LEN {
        return invalid("exceeds 64 chars");
    }
    if has_control(value) {
        return invalid("contains control characters");
    }
    Ok(value.to_owned())
}

// ---------------------------------------------------------------------------
// Spool file naming & visibility (ADR-0019 cl. 2)
// ---------------------------------------------------------------------------

/// Parsed components of a conforming spool file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolRef {
    /// Writer monotonic counter (per-writer FIFO under lexicographic sort).
    pub seq: u64,
    /// Writer slot UID (attribution; the daemon cross-checks file owner).
    pub writer: u32,
    /// Collision nonce (lowercase hex, 1–16 chars).
    pub nonce: String,
}

/// Build the conforming spool file name `{seq:012}-{writer}-{nonce}.mail`.
///
/// # Errors
///
/// [`MailError::InvalidFrom`] when `nonce` is not 1–16 lowercase hex chars.
pub fn spool_file_name(seq: u64, writer: u32, nonce: &str) -> Result<String, MailError> {
    let ok = !nonce.is_empty()
        && nonce.len() <= 16
        && nonce
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if !ok {
        return Err(MailError::InvalidFrom {
            value: format!("spool nonce {nonce:?} must be 1-16 lowercase hex chars"),
        });
    }
    Ok(format!("{seq:012}-{writer}-{nonce}.{SPOOL_EXT}"))
}

/// Parse a conforming spool file name; `None` for foreign names (which are
/// still deliverable mail — the grammar only buys FIFO ordering).
#[must_use]
pub fn parse_spool_file_name(name: &str) -> Option<SpoolRef> {
    let stem = name.strip_suffix(&format!(".{SPOOL_EXT}"))?;
    let mut parts = stem.split('-');
    let seq = parts.next()?.parse::<u64>().ok()?;
    let writer = parts.next()?.parse::<u32>().ok()?;
    let nonce = parts.next()?.to_owned();
    if parts.next().is_some() {
        return None;
    }
    spool_file_name(seq, writer, &nonce).ok()?;
    Some(SpoolRef { seq, writer, nonce })
}

/// True when a spool-dir entry is incoming mail: not hidden, not a
/// directory name we own (`.read`), and ending in `.mail`.
#[must_use]
pub fn is_incoming(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(&format!(".{SPOOL_EXT}"))
}

/// Sanitize a spool file name for tree accessors (single path segment).
///
/// # Errors
///
/// Propagates [`crate::sanitize::SanitizeError`] for non-conforming names.
pub fn safe_spool_name(name: &str) -> Result<SafeName, crate::sanitize::SanitizeError> {
    sanitize_name(name)
}

// ---------------------------------------------------------------------------
// Bounces (ADR-0019 cl. 4)
// ---------------------------------------------------------------------------

/// Typed undeliverable reason. The daemon renders a `From: system` bounce
/// mail from this plus the consumed original; the reason string is what
/// lands in the bounce body and the sled trace (no message content beyond
/// the original headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BounceReason {
    /// Recipient UID has no charter slot.
    NoSlot { uid: u32 },
    /// Recipient session cannot accept delivery in its current state.
    SessionUnavailable { uid: u32, state: String },
    /// Recipient delivery queue is full (Sprint 03 overflow hook).
    QueueOverflow { uid: u32, depth: u32 },
    /// Spool file failed [`Mail::parse`]; `detail` is the error string.
    Malformed { detail: String },
}

impl fmt::Display for BounceReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSlot { uid } => write!(f, "no slot allocated for UID {uid}"),
            Self::SessionUnavailable { uid, state } => {
                write!(f, "session for UID {uid} unavailable (state: {state})")
            }
            Self::QueueOverflow { uid, depth } => {
                write!(f, "delivery queue for UID {uid} full (depth {depth})")
            }
            Self::Malformed { detail } => write!(f, "malformed mail: {detail}"),
        }
    }
}

impl BounceReason {
    /// The `Subject:` of the bounce mail (bounded, single-line).
    #[must_use]
    pub fn bounce_subject(original_subject: Option<&str>) -> String {
        let original = original_subject.unwrap_or("(unparseable)");
        let budget = MAX_SUBJECT_LEN.saturating_sub("Bounce: ".chars().count());
        let truncated: String = original.chars().take(budget).collect();
        format!("Bounce: {truncated}")
    }
}

// ---------------------------------------------------------------------------
// Golden vectors (CONVENTIONS §9 — committed contract)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Committed golden vector: the exact canonical bytes of one mail.
    #[test]
    fn mail_render_golden() {
        let mail = Mail {
            header: MailHeader {
                from: MailSender::Uid(1042),
                to: 1001,
                subject: "Report ready".to_owned(),
                date: "2025-06-01T12:00:00Z".to_owned(),
            },
            body: "Findings in /var/freight/report-7.\nSecond line.\n".to_owned(),
        };
        let expected = "From: 1042\n\
                        To: 1001\n\
                        Subject: Report ready\n\
                        Date: 2025-06-01T12:00:00Z\n\
                        \n\
                        Findings in /var/freight/report-7.\nSecond line.\n";
        assert_eq!(mail.render(), expected);
    }

    /// Golden vector: named (non-slot) sender round-trips.
    #[test]
    fn mail_named_sender_roundtrip() {
        let raw =
            "From: patron-anna\nTo: 1000\nSubject: Question\nDate: 2025-06-01T12:00:00Z\n\nBody.\n";
        let mail = Mail::parse(raw).expect("valid");
        assert_eq!(
            mail.header.from,
            MailSender::Name(sanitize_name("patron-anna").unwrap())
        );
        assert_eq!(mail.render() + "\n", raw);
        // byte-exact round-trip: render(parse(x)) == x for canonical x
        let canonical = mail.render();
        assert_eq!(Mail::parse(&canonical).unwrap().render(), canonical);
    }

    #[test]
    fn mail_roundtrip_trailing_newline_optional() {
        let canonical = "From: 1000\nTo: 1001\nSubject: Hi\nDate: 2025-06-01T12:00:00Z\n\nBody.";
        assert_eq!(Mail::parse(canonical).unwrap().render(), canonical);
        assert_eq!(
            Mail::parse(&format!("{canonical}\n")).unwrap().render(),
            canonical
        );
    }

    /// Adversarial: header injection via CR, embedded newlines, missing and
    /// misordered headers, bad UIDs, empty/oversized parts.
    #[test]
    fn mail_rejections() {
        let base = "From: 1042\nTo: 1001\nSubject: S\nDate: D\n\nBody.\n";
        assert_eq!(Mail::parse(""), Err(MailError::Empty));
        assert_eq!(
            Mail::parse(&base.replace("Subject: S", "Subject: S\r\nX-Evil: 1")),
            Err(MailError::CarriageReturn)
        );
        // newline inside subject splits the line → header 3 (Date) mismatch
        let injected = "From: 1042\nTo: 1001\nSubject: S\nX-Evil: 1\nDate: D\n\nBody.\n";
        assert!(matches!(
            Mail::parse(injected),
            Err(MailError::HeaderMissing { index: 3 })
        ));
        let swapped = "To: 1001\nFrom: 1042\nSubject: S\nDate: D\n\nBody.\n";
        assert!(matches!(
            Mail::parse(swapped),
            Err(MailError::HeaderMissing { index: 0 })
        ));
        assert_eq!(
            Mail::parse("From: 1042\nTo: 1001\nSubject: S\nDate: D\nBody.\n"),
            Err(MailError::NoSeparator)
        );
        assert!(matches!(
            Mail::parse(&base.replace("From: 1042", "From: ../../etc")),
            Err(MailError::InvalidFrom { .. })
        ));
        assert!(matches!(
            Mail::parse(&base.replace("From: 1042", "From: 4000")),
            Err(MailError::InvalidFrom { .. })
        ));
        assert!(matches!(
            Mail::parse(&base.replace("To: 1001", "To: 2001")),
            Err(MailError::InvalidTo { .. })
        ));
        assert!(matches!(
            Mail::parse(&base.replace("Subject: S", "Subject:")),
            Err(MailError::HeaderMissing { index: 2 })
        ));
        assert!(matches!(
            Mail::parse(&base.replace("Subject: S", &format!("Subject: {}", "x".repeat(129)))),
            Err(MailError::InvalidSubject { .. })
        ));
        assert!(matches!(
            Mail::parse(&base.replace("Date: D", "Date:")),
            Err(MailError::HeaderMissing { index: 3 })
        ));
        assert!(matches!(
            Mail::parse(&base.replace("Date: D", &format!("Date: {}", "x".repeat(65)))),
            Err(MailError::InvalidDate { .. })
        ));
        assert_eq!(
            Mail::parse("From: 1042\nTo: 1001\nSubject: S\nDate: D\n\n"),
            Err(MailError::EmptyBody)
        );
        assert!(matches!(
            Mail::parse(&base.replace("Body.\n", &format!("{}\n", "y".repeat(MAX_BODY_BYTES + 1)))),
            Err(MailError::BodyTooLarge { .. })
        ));
    }

    /// Golden vectors: spool name grammar + FIFO sort order.
    #[test]
    fn spool_names_golden() {
        assert_eq!(
            spool_file_name(42, 1042, "ab12cd34").unwrap(),
            "000000000042-1042-ab12cd34.mail"
        );
        let parsed = parse_spool_file_name("000000000042-1042-ab12cd34.mail").unwrap();
        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.writer, 1042);
        assert_eq!(parsed.nonce, "ab12cd34");
        assert!(parse_spool_file_name("notes.mail").is_none());
        assert!(parse_spool_file_name("000000000042-1042-ZZZZ.mail").is_none());
        assert!(spool_file_name(1, 1042, "toolongtoolongtoolong").is_err());

        // Lexicographic order == seq order per writer (FIFO contract).
        let mut names: Vec<String> = (0..200u64)
            .map(|i| spool_file_name(199 - i, 1042, "ff").unwrap())
            .collect();
        names.sort();
        let seqs: Vec<u64> = names
            .iter()
            .map(|n| parse_spool_file_name(n).unwrap().seq)
            .collect();
        assert_eq!(seqs, (0..200).collect::<Vec<_>>());
    }

    #[test]
    fn incoming_visibility_rules() {
        assert!(is_incoming("000000000001-1042-ab.mail"));
        assert!(is_incoming("anything.mail"));
        assert!(!is_incoming(".tmp-ab12.mail"));
        assert!(!is_incoming(".read"));
        assert!(!is_incoming("notes.txt"));
        assert!(!is_incoming("mail"));
    }

    #[test]
    fn bounce_subject_bounded_and_single_line() {
        let long = "x".repeat(300);
        let subject = BounceReason::bounce_subject(Some(&long));
        assert_eq!(subject.chars().count(), MAX_SUBJECT_LEN);
        assert!(subject.starts_with("Bounce: "));
        assert_eq!(BounceReason::bounce_subject(None), "Bounce: (unparseable)");
        assert_eq!(
            BounceReason::QueueOverflow {
                uid: 1001,
                depth: 64
            }
            .to_string(),
            "delivery queue for UID 1001 full (depth 64)"
        );
    }

    #[test]
    fn slot_uid_range() {
        assert!(is_slot_uid(1000));
        assert!(is_slot_uid(1001));
        assert!(is_slot_uid(2000));
        assert!(!is_slot_uid(999));
        assert!(!is_slot_uid(2001));
        assert!(!is_slot_uid(0));
    }
}
