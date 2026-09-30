//! Adversarial suite — sanitizer escapes & API-boundary traversal
//! (Sprint 01, prompt 05-qa-engineer, items 1–2).
//!
//! SPEC §11.3 / §2 name-safety / CONVENTIONS §4 / ADR-0002. These tests go
//! BEYOND the committed golden vectors in `sanitize.rs`: hostile input
//! classes (traversal, absolute paths, NUL, unicode homoglyphs, overlength,
//! leading dash), an exhaustive short-string sweep over a hostile alphabet,
//! and the round-trip contract (every accept survives
//! `sanitize → as_str → re-sanitize → serde` unchanged; every reject is a
//! typed [`SanitizeError`]).
//!
//! **Item 2 — type-level proof (recorded choice):** compile-fail testing via
//! `trybuild` was REJECTED because `trybuild` is not on the locked
//! dev-dependency allowlist (`scripts/check-deps.sh`; adding it needs an
//! ADR). Instead SafeName's boundary is enforced two ways, both committed:
//! 1. Documented type-level proof (module `boundary_proof` below): the
//!    inner field of `SafeName` is private, `sanitize_name` is the only
//!    public constructor, serde `Deserialize` re-validates, and every
//!    name-taking tree accessor requires `&SafeName`.
//! 2. Real compile-FAIL probes compiled by `rustc` in
//!    `scripts/e2e-01-safename-compile-fail.sh` (raw `&str` into
//!    `vault_artifact`/`courier_mail_recipient`/`patron`, and direct
//!    `SafeName(..)` construction, must all fail to compile).

use std::path::Path;

use hamlet_core::sanitize::{sanitize_name, SafeName, SanitizeError, MAX_NAME_LEN};

/// Every documented rejection class from the QA prompt, with the exact
/// typed error (check order per ADR-0002: Empty → TooLong → PathSeparator
/// → ControlChar → NonAscii → LeadingDash → DotName → InvalidShape).
#[test]
fn hostile_classes_get_typed_errors() {
    let cases: &[(&str, SanitizeError)] = &[
        // --- traversal / separators ---
        ("../", SanitizeError::PathSeparator),
        ("../../etc/passwd", SanitizeError::PathSeparator),
        ("..\\..\\windows\\system32", SanitizeError::PathSeparator),
        ("/etc/passwd", SanitizeError::PathSeparator),
        ("//server/share", SanitizeError::PathSeparator),
        ("/", SanitizeError::PathSeparator),
        ("\\", SanitizeError::PathSeparator),
        ("a/../b", SanitizeError::PathSeparator),
        ("....//", SanitizeError::PathSeparator),
        // percent-/url-encoded traversal is NOT decoded — raw charset rules
        ("..%2f", SanitizeError::InvalidShape),
        ("..%2F..%2Fetc", SanitizeError::InvalidShape),
        ("..%252f", SanitizeError::InvalidShape),
        ("C:\\abs", SanitizeError::PathSeparator),
        // --- NUL & control chars ---
        ("\0", SanitizeError::ControlChar),
        ("ada\0", SanitizeError::ControlChar),
        ("\0ada", SanitizeError::ControlChar),
        ("a\0b", SanitizeError::ControlChar),
        ("esc\x1b[31m", SanitizeError::ControlChar),
        ("cr\rname", SanitizeError::ControlChar),
        ("del\x7f", SanitizeError::ControlChar),
        // C1 controls count as control chars (char::is_control), checked
        // before NonAscii per the locked order.
        ("nel\u{85}", SanitizeError::ControlChar),
        // --- unicode homoglyphs & invisibles ---
        ("аdmin", SanitizeError::NonAscii),  // Cyrillic а U+0430
        ("ａdmin", SanitizeError::NonAscii), // fullwidth ａ U+FF41
        ("admın", SanitizeError::NonAscii),  // dotless ı U+0131
        ("adмин", SanitizeError::NonAscii),  // Cyrillic мин
        ("a\u{2215}b", SanitizeError::NonAscii), // DIVISION SLASH lookalike
        ("a\u{2044}b", SanitizeError::NonAscii), // FRACTION SLASH lookalike
        ("a\u{FF0F}b", SanitizeError::NonAscii), // FULLWIDTH SOLIDUS
        ("a\u{202E}b", SanitizeError::NonAscii), // RTL override (bidi attack)
        ("ad\u{200B}min", SanitizeError::NonAscii), // zero-width space
        ("admin\u{FEFF}", SanitizeError::NonAscii), // BOM / ZWNBSP
        ("ﬀile", SanitizeError::NonAscii),   // ligature ff U+FB00
        ("ｅｘａｃｔｌｙ", SanitizeError::NonAscii), // fullwidth word
        // --- length ---
        (
            &"a".repeat(MAX_NAME_LEN + 1),
            SanitizeError::TooLong { len: 65 },
        ),
        (&"あ".repeat(65), SanitizeError::TooLong { len: 65 }), // char count, not bytes
        (
            &"a".repeat(100_000),
            SanitizeError::TooLong { len: 100_000 },
        ),
        // --- leading dash (CLI flag injection) ---
        ("-", SanitizeError::LeadingDash),
        ("-rf", SanitizeError::LeadingDash),
        ("--help", SanitizeError::LeadingDash),
        ("-..", SanitizeError::LeadingDash),
        // --- dot names & shape ---
        ("", SanitizeError::Empty),
        (".", SanitizeError::DotName),
        ("..", SanitizeError::DotName),
        (".hidden", SanitizeError::InvalidShape),
        ("hidden.", SanitizeError::InvalidShape),
        ("a..b", SanitizeError::InvalidShape),
        ("tar.gz.bak", SanitizeError::InvalidShape),
        ("~tilde", SanitizeError::InvalidShape),
        ("$HOME", SanitizeError::InvalidShape),
        ("`rm -rf`", SanitizeError::InvalidShape),
        ("name\n;rm", SanitizeError::ControlChar),
    ];
    for (input, expected) in cases {
        let got = sanitize_name(input);
        assert_eq!(
            got.unwrap_err(),
            *expected,
            "wrong typed error for hostile input {input:?}"
        );
    }
}

/// Boundary lengths: 64 chars is the inclusive upper bound; 63/65 pin it.
#[test]
fn length_boundary_is_inclusive_at_64() {
    assert!(sanitize_name(&"a".repeat(MAX_NAME_LEN)).is_ok());
    assert!(sanitize_name(&"a".repeat(MAX_NAME_LEN - 1)).is_ok());
    assert_eq!(
        sanitize_name(&"a".repeat(MAX_NAME_LEN + 1)),
        Err(SanitizeError::TooLong {
            len: MAX_NAME_LEN + 1
        })
    );
    // A 63-char name with the single permitted dot-extension still fits.
    let mut dotted = "b".repeat(MAX_NAME_LEN - 5);
    dotted.push_str(".tar");
    assert_eq!(dotted.chars().count(), MAX_NAME_LEN - 1);
    assert!(sanitize_name(&dotted).is_ok());
}

/// Every ACCEPT round-trips: `as_str` equals the input byte-for-byte,
/// re-sanitizing is stable, serde string round-trip preserves the value,
/// and the name is always a single path segment that cannot escape a join.
#[test]
fn accepts_roundtrip_and_stay_single_segment() {
    let accepts = [
        "a",
        "Z",
        "0",
        "agent-01",
        "agent_01",
        "PROPRIETOR.mayor",
        "SKILL.md",
        "ledger-2026_01",
        "a-b_c.d",
        "0000",
        "f.ext",
        "report-final_v2.pdf",
        &"x".repeat(MAX_NAME_LEN),
    ];
    for input in accepts {
        let safe =
            sanitize_name(input).unwrap_or_else(|e| panic!("{input:?} must be accepted: {e}"));
        // Round-trip 1: string identity.
        assert_eq!(safe.as_str(), input);
        assert_eq!(safe.to_string(), input);
        assert_eq!(safe.into_string().clone(), input);
        // Round-trip 2: re-sanitize stability.
        let safe2 = sanitize_name(&sanitize_name(input).unwrap().into_string()).unwrap();
        assert_eq!(safe2.as_str(), input);
        // Round-trip 3: serde string form.
        let json = serde_json::to_string(&sanitize_name(input).unwrap()).unwrap();
        assert_eq!(json, format!("{input:?}"));
        let back: SafeName = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), input);
        // Single-segment: joining under a root never changes the parent.
        let root = Path::new("/village/keep/vault");
        let joined = root.join(sanitize_name(input).unwrap().as_str());
        assert_eq!(joined.parent(), Some(root), "escape via {input:?}");
        assert_eq!(joined.file_name().and_then(|s| s.to_str()), Some(input));
    }
}

/// Serde is a trust boundary: hostile names embedded in JSON (a lying
/// charter, a webhook payload) are rejected at deserialization, and the
/// error names the offending value.
#[test]
fn serde_deserialize_rejects_hostile_names() {
    for hostile in [
        "\"../evil\"",
        "\"/abs\"",
        "\"\"",
        "\"nul\\u0000name\"",
        "\" leading\"",
        "\"trailing.\"",
        "\"multi.dot.name\"",
        "\"\\u0430dmin\"",
        "\"-x\"",
        "\".\"",
        "\"..\"",
    ] {
        let result = serde_json::from_str::<SafeName>(hostile);
        assert!(result.is_err(), "serde accepted hostile name {hostile}");
    }
    // A hostile SafeName inside a nested structure is rejected too.
    let lying = r#"{"1001": {"identity": "../evil", "status": "active"}}"#;
    let parsed: Result<std::collections::BTreeMap<u32, hamlet_core::slots::SlotRecord>, _> =
        serde_json::from_str(lying);
    assert!(parsed.is_err());
}

/// Exhaustive sweep (QA prompt item 1: "every reject typed, every accept
/// round-trips"): ALL strings of length 0..=3 over a hostile alphabet.
/// For each candidate exactly one of the contracts must hold:
/// - `Ok`: input non-empty, ≤64 chars, single path segment, not `.`/`..`,
///   no separator/control/non-ascii char, doesn't start with `-`/`.`,
///   round-trips through `as_str` and serde, and re-sanitizes identically.
/// - `Err`: a typed `SanitizeError` variant with a non-empty `Display`.
#[test]
fn exhaustive_short_string_sweep_over_hostile_alphabet() {
    const ALPHABET: &[char] = &[
        'a', 'B', '9', '.', '-', '_', '/', '\\', '\0', ':', '~', ' ', '\u{e9}',  // é
        '\u{430}', // Cyrillic а
        '*',
    ];
    let mut accepts = 0usize;
    let mut rejects = 0usize;

    let mut buf: Vec<char> = Vec::new();
    fn rec(
        buf: &mut Vec<char>,
        alpha: &[char],
        depth: u32,
        accepts: &mut usize,
        rejects: &mut usize,
    ) {
        if depth > 3 {
            return;
        }
        if !buf.is_empty() || depth > 0 {
            check(&buf.iter().collect::<String>(), accepts, rejects);
        }
        if depth == 3 {
            return;
        }
        for &c in alpha {
            buf.push(c);
            rec(buf, alpha, depth + 1, accepts, rejects);
            buf.pop();
        }
    }
    fn check(s: &str, accepts: &mut usize, rejects: &mut usize) {
        match sanitize_name(s) {
            Ok(safe) => {
                *accepts += 1;
                assert_eq!(safe.as_str(), s, "accept must round-trip");
                assert!(!s.is_empty() && s.chars().count() <= MAX_NAME_LEN);
                assert!(!s.contains(['/', '\\']));
                assert!(!s.chars().any(char::is_control));
                assert!(s.is_ascii(), "accepted name must be ASCII: {s:?}");
                assert!(!s.starts_with('-') && !s.starts_with('.'));
                assert!(s != "." && s != "..");
                let joined = Path::new("/root").join(s);
                assert_eq!(joined.parent(), Some(Path::new("/root")), "escape: {s:?}");
                let again = sanitize_name(safe.as_str()).unwrap();
                assert_eq!(again, safe, "re-sanitize must be stable");
                let json = serde_json::to_string(&safe).unwrap();
                assert_eq!(serde_json::from_str::<SafeName>(&json).unwrap(), safe);
            }
            Err(e) => {
                *rejects += 1;
                // Typed: one of the documented variants (exhaustive match).
                match e {
                    SanitizeError::Empty
                    | SanitizeError::TooLong { .. }
                    | SanitizeError::PathSeparator
                    | SanitizeError::ControlChar
                    | SanitizeError::NonAscii
                    | SanitizeError::LeadingDash
                    | SanitizeError::DotName
                    | SanitizeError::InvalidShape => {}
                }
                assert!(!e.to_string().is_empty(), "error must be printable");
                // The error is a real std::error::Error.
                let dyn_err: &dyn std::error::Error = &e;
                assert!(!dyn_err.to_string().is_empty());
            }
        }
    }
    rec(&mut buf, ALPHABET, 0, &mut accepts, &mut rejects);
    // Sanity: the sweep is meaningfully large and mixed.
    assert!(
        accepts > 100,
        "sweep should accept a substantial set: {accepts}"
    );
    assert!(
        rejects > 1000,
        "sweep should reject a substantial set: {rejects}"
    );
}

/// Documented type-level proof (QA prompt item 2, recorded choice — the
/// compile-FAIL half lives in `scripts/e2e-01-safename-compile-fail.sh`).
///
/// The proof obligations, checked here where stable Rust allows and by the
/// rustc probes otherwise:
/// 1. `SafeName`'s inner `String` is a PRIVATE tuple field — code outside
///    `hamlet_core::sanitize` cannot construct one (probe: direct
///    `SafeName("evil".into())` must fail with E0423/E0451).
/// 2. `sanitize_name` is the ONLY public constructor — there is no
///    `From<&str>`, `From<String>`, or `TryFrom` impl on `SafeName`
///    (probe: `SafeName::from("x")` must fail to resolve).
/// 3. Name-taking tree accessors (`patron`, `vault_artifact`,
///    `courier_mail_recipient`) take `&SafeName`, not `&str`/`AsRef<str>`
///    generics (probes: raw string calls must fail with E0308).
/// 4. Deserialization re-validates (runtime half proven by the serde tests
///    above).
///
/// What this module proves at runtime: `SafeName` hands out only `&str`
/// views — no mutable access, no `DerefMut`, no `String` leak without
/// consuming the value — so a validated name can never be modified into an
/// unsafe one after the fact.
mod boundary_proof {
    use hamlet_core::sanitize::sanitize_name;

    #[test]
    fn safename_exposes_immutable_views_only() {
        let safe = sanitize_name("agent-01").unwrap();
        // &str view reflects the validated value exactly.
        let view: &str = safe.as_str();
        assert_eq!(view, "agent-01");
        // AsRef<str> agrees.
        let aref: &str = safe.as_ref();
        assert_eq!(aref, "agent-01");
        // Cloning preserves the value; equality is value equality.
        assert_eq!(safe.clone(), sanitize_name("agent-01").unwrap());
        // Display matches the validated string (no escaping surprises).
        assert_eq!(safe.to_string(), "agent-01");
        // into_string consumes — after which the SafeName no longer exists
        // to be misused; the returned String cannot re-enter the type
        // without passing sanitize_name again.
        let owned: String = safe.into_string(); // consumes — `safe` no longer exists
        assert_eq!(owned, "agent-01");
    }

    #[test]
    fn ordering_is_by_validated_value_only() {
        // BTreeMap/BTreeSet keys (charter state) sort by the validated
        // string — no interior mutability, no custom Ord surprises.
        let a = sanitize_name("ada").unwrap();
        let b = sanitize_name("grace").unwrap();
        assert!(a < b);
        let mut set = std::collections::BTreeSet::new();
        set.insert(b.clone());
        set.insert(a.clone());
        set.insert(b.clone());
        assert_eq!(set.len(), 2);
        assert_eq!(set.iter().next(), Some(&a));
    }
}
