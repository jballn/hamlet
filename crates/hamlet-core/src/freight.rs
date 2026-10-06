//! Freight POSIX ACL sandbox vocabulary (SPEC §4 "Freight ACLs", §8.3;
//! ADR-0021).
//!
//! Pure logic for the daemon-side ACL audit: a parser for `getfacl -p -n`
//! listings (the daemon obtains them through `podman unshare getfacl`, so the
//! numeric ids are the in-userns slot UIDs) and the audit rules themselves.
//!
//! Audit policy (ADR-0021 cl. 3–4) — a freight sandbox is *sane* when:
//!
//! 1. its owner is a valid charter slot UID;
//! 2. neither its access nor its default ACL grants `other` write
//!    (no world-writable sandboxes — "no `other::rw`");
//! 3. every named ACL grantee UID is inside the commission-scoped roster
//!    (grantees outside the roster are foreign — sharing is commission-
//!    scoped by policy);
//! 4. no named *group* entries exist at all — group membership is not
//!    charter state, so a group grant cannot be roster-checked (UID-scoped
//!    sharing only).
//!
//! Violations are typed values with render-stable `kind` strings; the daemon
//! records them in sled traces and quarantine flag files. This module never
//! touches the filesystem — listings arrive as bytes (CONVENTIONS §2 purity
//! seam), which makes the audit unit-testable over committed golden fixtures
//! (CONVENTIONS §9).

use std::collections::BTreeSet;
use std::fmt;

/// The three POSIX permission bits of one ACL entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Perms {
    /// `r`
    pub read: bool,
    /// `w`
    pub write: bool,
    /// `x`
    pub execute: bool,
}

impl Perms {
    /// `---`
    pub const NONE: Self = Self {
        read: false,
        write: false,
        execute: false,
    };

    /// `rwx`
    pub const ALL: Self = Self {
        read: true,
        write: true,
        execute: true,
    };

    /// Parse the `getfacl` permission field: exactly three chars from
    /// `[r-][w-][x-]`.
    ///
    /// # Errors
    ///
    /// [`FreightError::MalformedPerms`] on any other shape.
    pub fn parse(text: &str) -> Result<Self, FreightError> {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() != 3 {
            return Err(FreightError::MalformedPerms {
                value: text.to_owned(),
            });
        }
        let bit = |c: char, expected: char| -> Result<bool, FreightError> {
            if c == expected {
                Ok(true)
            } else if c == '-' {
                Ok(false)
            } else {
                Err(FreightError::MalformedPerms {
                    value: text.to_owned(),
                })
            }
        };
        Ok(Self {
            read: bit(chars[0], 'r')?,
            write: bit(chars[1], 'w')?,
            execute: bit(chars[2], 'x')?,
        })
    }

    /// Render back to the `getfacl` field form (`rwx`, `r-x`, `---`, …).
    #[must_use]
    pub fn render(self) -> String {
        format!(
            "{}{}{}",
            if self.read { 'r' } else { '-' },
            if self.write { 'w' } else { '-' },
            if self.execute { 'x' } else { '-' }
        )
    }
}

impl fmt::Display for Perms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// The tag of one ACL entry. `None` qualifiers are the base (owning
/// user/group) entries; `Some(uid)` are named grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Qualifier {
    /// `user::` (owner base) or `user:<uid>:` (named grant).
    User(Option<u32>),
    /// `group::` (owning group base) or `group:<gid>:` (named grant).
    Group(Option<u32>),
    /// `mask::` — the effective-rights ceiling of named entries.
    Mask,
    /// `other::`
    Other,
}

/// One ACL entry: tag + permission triplet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclEntry {
    /// Entry tag (`user::`, `user:1002:`, `mask::`, …).
    pub qualifier: Qualifier,
    /// Permission bits as listed (pre-mask; the audit judges listed perms —
    /// a masked-away write is still a *declared* share, and the mask can be
    /// re-raised by the owner at any time).
    pub perms: Perms,
}

/// A parsed `getfacl -p -n` listing of one filesystem object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclListing {
    /// `# owner:` — numeric UID (in-userns slot UID via `podman unshare`).
    pub owner: u32,
    /// `# group:` — numeric GID.
    pub group: u32,
    /// Access ACL entries, in listing order.
    pub access: Vec<AclEntry>,
    /// `default:` ACL entries (directories only), in listing order.
    pub default: Vec<AclEntry>,
}

/// Typed parse/audit failures. Malformed listings are corruption-or-hostile
/// input: never silently tolerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreightError {
    /// CR anywhere in the listing (injection defense, mirrors the mail
    /// format's LF-only rule).
    CarriageReturn,
    /// Missing `# owner:` comment.
    MissingOwner,
    /// Missing `# group:` comment.
    MissingGroup,
    /// No base `other::` entry — not a valid getfacl dump.
    MissingOther,
    /// An entry line that does not fit the `tag[:qualifier]:perms` grammar.
    MalformedEntry {
        /// The offending line (verbatim).
        line: String,
    },
    /// A non-numeric owner/group/named-qualifier id (`-n` guarantees
    /// numeric output; anything else is hostile).
    NonNumericId {
        /// The offending value.
        value: String,
    },
    /// A permission field outside `[r-][w-][x-]`.
    MalformedPerms {
        /// The offending value.
        value: String,
    },
}

impl fmt::Display for FreightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CarriageReturn => write!(f, "ACL listing contains CR"),
            Self::MissingOwner => write!(f, "ACL listing has no '# owner:' line"),
            Self::MissingGroup => write!(f, "ACL listing has no '# group:' line"),
            Self::MissingOther => write!(f, "ACL listing has no base 'other::' entry"),
            Self::MalformedEntry { line } => write!(f, "malformed ACL entry: {line:?}"),
            Self::NonNumericId { value } => write!(f, "non-numeric ACL id: {value:?}"),
            Self::MalformedPerms { value } => write!(f, "malformed ACL perms: {value:?}"),
        }
    }
}

impl std::error::Error for FreightError {}

/// Parse one `getfacl -p -n --` listing (comment header present: the `# owner:`
/// line is state, not noise — ADR-0031). Strict: CR is rejected, the
/// `# owner:`/`# group:` comments and a base `other::` entry are mandatory,
/// trailing `# effective:` comments are stripped, anything else malformed is
/// a typed error.
///
/// # Errors
///
/// [`FreightError`] on any grammar violation.
pub fn parse_getfacl(text: &str) -> Result<AclListing, FreightError> {
    if text.contains('\r') {
        return Err(FreightError::CarriageReturn);
    }
    let mut owner: Option<u32> = None;
    let mut group: Option<u32> = None;
    let mut access = Vec::new();
    let mut default = Vec::new();
    for raw in text.lines() {
        // Strip the trailing "\t# effective:…" annotation getfacl adds when
        // the mask limits a named entry.
        let line = match raw.find('\t') {
            Some(idx) => &raw[..idx],
            None => raw,
        };
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix("# ") {
            if let Some(value) = comment.strip_prefix("owner: ") {
                owner = Some(parse_id(value)?);
            } else if let Some(value) = comment.strip_prefix("group: ") {
                group = Some(parse_id(value)?);
            }
            // `# file:`, `# flags:`, … carry no audit-relevant state.
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let (is_default, body) = match line.strip_prefix("default:") {
            Some(body) => (true, body),
            None => (false, line),
        };
        let entry = parse_entry(body, line)?;
        if is_default {
            default.push(entry);
        } else {
            access.push(entry);
        }
    }
    let listing = AclListing {
        owner: owner.ok_or(FreightError::MissingOwner)?,
        group: group.ok_or(FreightError::MissingGroup)?,
        access,
        default,
    };
    if !listing
        .access
        .iter()
        .any(|e| e.qualifier == Qualifier::Other)
    {
        return Err(FreightError::MissingOther);
    }
    Ok(listing)
}

fn parse_id(value: &str) -> Result<u32, FreightError> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| FreightError::NonNumericId {
            value: value.to_owned(),
        })
}

fn parse_entry(body: &str, line: &str) -> Result<AclEntry, FreightError> {
    let malformed = || FreightError::MalformedEntry {
        line: line.to_owned(),
    };
    let parts: Vec<&str> = body.split(':').collect();
    let (tag, qualifier, perms) = match parts.as_slice() {
        [tag, perms] => (*tag, None, *perms),
        [tag, qual, perms] => (*tag, Some(*qual), *perms),
        _ => return Err(malformed()),
    };
    let named = match qualifier {
        None => None,
        Some("") => None,
        Some(value) => Some(parse_id(value).map_err(|_| FreightError::NonNumericId {
            value: value.to_owned(),
        })?),
    };
    let qualifier = match tag {
        "user" => Qualifier::User(named),
        "group" => Qualifier::Group(named),
        "mask" if named.is_none() => Qualifier::Mask,
        "other" if named.is_none() => Qualifier::Other,
        _ => return Err(malformed()),
    };
    Ok(AclEntry {
        qualifier,
        perms: Perms::parse(perms)?,
    })
}

/// Typed audit violations. `kind_str` is the render-stable identity used in
/// sled traces and quarantine flag files — never renumber, only add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ViolationKind {
    /// The sandbox owner is not a valid charter slot UID (rule 1).
    OwnerNotSlot,
    /// `other` (access or default) carries write (rule 2: no `other::rw`,
    /// no world-writable sandboxes).
    WorldWritable,
    /// A named ACL grantee UID is outside the commission-scoped roster
    /// (rule 3).
    ForeignUid,
    /// A named *group* entry exists (rule 4: UID-scoped sharing only —
    /// group membership is not charter state).
    NamedGroup,
    /// The freight entry is not a real directory (file/symlink at the
    /// sandbox level — daemon-detected via lstat, never via the listing).
    NotADirectory,
}

impl ViolationKind {
    /// The stable flag/trace identity of this kind.
    #[must_use]
    pub fn kind_str(self) -> &'static str {
        match self {
            Self::OwnerNotSlot => "owner_not_slot",
            Self::WorldWritable => "world_writable",
            Self::ForeignUid => "foreign_uid",
            Self::NamedGroup => "named_group",
            Self::NotADirectory => "not_a_directory",
        }
    }
}

impl fmt::Display for ViolationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind_str())
    }
}

/// One audit finding: typed kind + human-readable detail (names/ids only —
/// never artifact content, CONVENTIONS §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Which rule broke.
    pub kind: ViolationKind,
    /// Render-stable detail (uid values, entry text).
    pub detail: String,
}

/// The roster context the audit judges against (ADR-0021 cl. 3). Both sets
/// are charter-derived by the daemon; this module only consumes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPolicy {
    /// UIDs that may *own* a sandbox: every charter slot (the Mayor
    /// included — the manor container mounts `/var/freight/` too).
    pub owners: BTreeSet<u32>,
    /// UIDs that may be *granted* access: the commission-scoped roster
    /// (Mayor + slots bound to an Open commission).
    pub grantees: BTreeSet<u32>,
}

/// Audit one parsed listing against the policy. Pure; returns every
/// violation found (not just the first), in a deterministic order:
/// owner → access entries → default entries, listing order within each.
#[must_use]
pub fn audit(listing: &AclListing, policy: &AuditPolicy) -> Vec<Violation> {
    let mut violations = Vec::new();
    if !policy.owners.contains(&listing.owner) {
        violations.push(Violation {
            kind: ViolationKind::OwnerNotSlot,
            detail: format!("owner uid {} is not a charter slot", listing.owner),
        });
    }
    for (scope, entries) in [("access", &listing.access), ("default", &listing.default)] {
        for entry in entries {
            match entry.qualifier {
                Qualifier::Other if entry.perms.write => violations.push(Violation {
                    kind: ViolationKind::WorldWritable,
                    detail: format!("{scope} other::{} grants write", entry.perms),
                }),
                Qualifier::User(Some(uid)) if !policy.grantees.contains(&uid) => {
                    violations.push(Violation {
                        kind: ViolationKind::ForeignUid,
                        detail: format!(
                            "{scope} user:{uid}:{} outside the commission roster",
                            entry.perms
                        ),
                    });
                }
                Qualifier::Group(Some(gid)) => violations.push(Violation {
                    kind: ViolationKind::NamedGroup,
                    detail: format!(
                        "{scope} group:{gid}:{} — UID-scoped sharing only",
                        entry.perms
                    ),
                }),
                _ => {}
            }
        }
    }
    violations
}

/// A typed `setfacl -m` grant token: `u:<uid>:<perms>`. Built from a
/// numeric UID and typed perms, so the rendered token is charset-safe by
/// construction (no injection surface — the daemon's argv builders accept
/// only these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclGrant {
    /// Grantee UID.
    pub uid: u32,
    /// Granted permission bits.
    pub perms: Perms,
}

impl AclGrant {
    /// A sandbox-creator grant on the freight root: `u:<uid>:rwx`.
    #[must_use]
    pub fn root_access(uid: u32) -> Self {
        Self {
            uid,
            perms: Perms::ALL,
        }
    }

    /// The `setfacl -m` spec token (`u:1001:rwx`).
    #[must_use]
    pub fn token(self) -> String {
        format!("u:{}:{}", self.uid, self.perms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> AuditPolicy {
        AuditPolicy {
            owners: BTreeSet::from([1000, 1001, 1002, 1003]),
            grantees: BTreeSet::from([1000, 1001, 1002]), // 1003: closed commission
        }
    }

    /// Golden fixture: a well-formed sandbox secured for one recipient
    /// (the shape `setfacl -m u::rwx,u:1002:rwx,o::---` produces).
    const CLEAN: &str = "\
# file: var/freight/exchange
# owner: 1001
# group: 1001
user::rwx
user:1002:rwx
group::r-x
mask::rwx
other::---
";

    /// Golden fixture: deliberately world-writable.
    const WORLD_WRITABLE: &str = "\
# file: var/freight/openbox
# owner: 1001
# group: 1001
user::rwx
group::r-x
other::rwx
";

    /// Golden fixture: foreign UID grant + named group + masked effective
    /// annotation + default entries.
    const MIXED: &str = "\
# file: var/freight/rogue
# owner: 31337
# group: 1001
user::rwx
user:4242:rwx\t#effective:r-x
group:77:r-x
mask::rwx
other::r--
default:user::rwx
default:user:1002:rwx
default:user:1003:rwx
default:other::rwx
";

    #[test]
    fn parse_clean_golden() {
        let listing = parse_getfacl(CLEAN).unwrap();
        assert_eq!(listing.owner, 1001);
        assert_eq!(listing.group, 1001);
        assert!(listing.default.is_empty());
        assert_eq!(
            listing.access,
            vec![
                AclEntry {
                    qualifier: Qualifier::User(None),
                    perms: Perms::ALL
                },
                AclEntry {
                    qualifier: Qualifier::User(Some(1002)),
                    perms: Perms::ALL
                },
                AclEntry {
                    qualifier: Qualifier::Group(None),
                    perms: Perms {
                        read: true,
                        write: false,
                        execute: true
                    }
                },
                AclEntry {
                    qualifier: Qualifier::Mask,
                    perms: Perms::ALL
                },
                AclEntry {
                    qualifier: Qualifier::Other,
                    perms: Perms::NONE
                },
            ]
        );
        assert!(audit(&listing, &policy()).is_empty());
    }

    #[test]
    fn parse_mixed_golden_strips_effective_and_splits_default() {
        let listing = parse_getfacl(MIXED).unwrap();
        assert_eq!(listing.owner, 31337);
        assert_eq!(
            listing.access[1],
            AclEntry {
                qualifier: Qualifier::User(Some(4242)),
                perms: Perms::ALL,
            },
            "effective annotation stripped; listed perms kept (re-raisable mask)"
        );
        assert_eq!(
            listing.default,
            vec![
                AclEntry {
                    qualifier: Qualifier::User(None),
                    perms: Perms::ALL
                },
                AclEntry {
                    qualifier: Qualifier::User(Some(1002)),
                    perms: Perms::ALL
                },
                AclEntry {
                    qualifier: Qualifier::User(Some(1003)),
                    perms: Perms::ALL
                },
                AclEntry {
                    qualifier: Qualifier::Other,
                    perms: Perms::ALL
                },
            ]
        );
    }

    #[test]
    fn parse_rejections_golden() {
        assert_eq!(
            parse_getfacl("# owner: 1\r\n"),
            Err(FreightError::CarriageReturn)
        );
        assert_eq!(
            parse_getfacl("user::rwx\nother::---\n"),
            Err(FreightError::MissingOwner)
        );
        assert_eq!(
            parse_getfacl("# owner: 1001\nuser::rwx\nother::---\n"),
            Err(FreightError::MissingGroup)
        );
        assert_eq!(
            parse_getfacl("# owner: 1001\n# group: 1001\nuser::rwx\n"),
            Err(FreightError::MissingOther)
        );
        assert_eq!(
            parse_getfacl("# owner: x\n# group: 1\nother::---\n"),
            Err(FreightError::NonNumericId {
                value: "x".to_owned()
            })
        );
        assert_eq!(
            parse_getfacl("# owner: 1\n# group: 1\nother::---\nuser:1002:rwx:extra\n"),
            Err(FreightError::MalformedEntry {
                line: "user:1002:rwx:extra".to_owned()
            })
        );
        assert_eq!(
            parse_getfacl("# owner: 1\n# group: 1\nother::rwq\n"),
            Err(FreightError::MalformedPerms {
                value: "rwq".to_owned()
            })
        );
    }

    /// The audit rule table as a golden: fixture → exact violation kinds.
    #[test]
    fn audit_rule_table_golden() {
        let cases: Vec<(&str, Vec<ViolationKind>)> = vec![
            (CLEAN, vec![]),
            (WORLD_WRITABLE, vec![ViolationKind::WorldWritable]),
            (
                MIXED,
                vec![
                    ViolationKind::OwnerNotSlot,  // 31337
                    ViolationKind::ForeignUid,    // user:4242 (access)
                    ViolationKind::NamedGroup,    // group:77
                    ViolationKind::ForeignUid,    // default user:1003 (closed commission)
                    ViolationKind::WorldWritable, // default other::rwx
                ],
            ),
        ];
        for (fixture, expected) in cases {
            let listing = parse_getfacl(fixture).unwrap();
            let kinds: Vec<ViolationKind> =
                audit(&listing, &policy()).iter().map(|v| v.kind).collect();
            assert_eq!(kinds, expected, "fixture mismatch:\n{fixture}");
        }
    }

    #[test]
    fn grant_tokens_render_setfacl_specs() {
        assert_eq!(AclGrant::root_access(1001).token(), "u:1001:rwx");
        assert_eq!(
            AclGrant {
                uid: 1002,
                perms: Perms {
                    read: true,
                    write: false,
                    execute: true
                }
            }
            .token(),
            "u:1002:r-x"
        );
    }

    #[test]
    fn perms_round_trip() {
        for text in ["rwx", "r-x", "-w-", "--x", "r--", "-w", "---"] {
            let expected = if text == "-w" {
                None
            } else {
                Some(text.to_owned())
            };
            match Perms::parse(text) {
                Ok(perms) => assert_eq!(Some(perms.render()), expected),
                Err(_) => assert!(expected.is_none(), "{text} should parse"),
            }
        }
    }
}
