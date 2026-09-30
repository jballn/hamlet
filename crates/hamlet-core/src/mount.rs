//! Normative container mount table (SPEC §4).
//!
//! Typed encoding of the exact host→container mount set every container
//! receives: parcel home, `/var/mail/`, `/var/freight/`, `/var/git/` (ro),
//! `/var/pkgcache/`, `/var/context/` (ro), manor home (Mayor only). The
//! daemon consumes this table verbatim; deviations and the §4 isolation
//! invariants are QA-enforced against it.
//!
//! # Closed by construction
//!
//! [`MountSpec`] is a sealed-shape enum: its variants carry only a UID or a
//! commission number — never a path. Host paths are derived exclusively
//! through [`HamletTree`] accessors (ADR-0003 typed-paths policy) and
//! container paths are the fixed SPEC §4 literals in [`MountSpec::container_path`].
//! A mount that is not a row of the SPEC §4 table is **unrepresentable**:
//! there is no public constructor from raw host or container paths.
//!
//! # Purity
//!
//! [`MountSet::to_podman_args`] renders `-v` flag strings only — it never
//! executes anything (ADR-0010: the daemon is the sole podman invoker and
//! consumes these args verbatim as a `Vec<String>`, never through a shell).

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;

use crate::slots::{FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};
use crate::tree::HamletTree;

/// Mount access mode (SPEC §4 "Mode" column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MountMode {
    /// Read-only bind (`:ro` suffix) — `/var/git/`, `/var/context/`.
    Ro,
    /// Read-write bind.
    Rw,
}

/// One row of the SPEC §4 mount table. No other mount is expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MountSpec {
    /// `keep/census/parcel<uid>/home/` → `/home/<uid>/` (rw).
    AgentHome { uid: u32 },
    /// `courier/mail/` → `/var/mail/` (rw; per-UID spool perms inside).
    Mail,
    /// `courier/freight/` → `/var/freight/` (rw; POSIX ACL sandboxes inside).
    Freight,
    /// `guild/ledger/commission<n>/git/` → `/var/git/` (**ro**).
    CommissionGit { commission: u64 },
    /// `market/pkgcache/` → `/var/pkgcache/` (rw, cache semantics).
    PkgCache,
    /// `tavern/gossip/` → `/var/context/` (**ro**).
    Context,
    /// `keep/census/manor/home/` → `/home/1000/` (rw; Mayor container only).
    ManorHome,
}

impl MountSpec {
    /// Host-side path, derived from the typed tree layout (never ad hoc).
    #[must_use]
    pub fn host_path(&self, tree: &HamletTree) -> PathBuf {
        match self {
            Self::AgentHome { uid } => tree.parcel_home(*uid),
            Self::Mail => tree.courier_mail(),
            Self::Freight => tree.courier_freight(),
            Self::CommissionGit { commission } => tree.commission_git(*commission),
            Self::PkgCache => tree.market_pkgcache(),
            Self::Context => tree.tavern_gossip(),
            Self::ManorHome => tree.manor_home(),
        }
    }

    /// Container-side path — the fixed SPEC §4 literal for this row.
    #[must_use]
    pub fn container_path(&self) -> Cow<'static, str> {
        match self {
            Self::AgentHome { uid } => Cow::Owned(format!("/home/{uid}/")),
            Self::Mail => Cow::Borrowed("/var/mail/"),
            Self::Freight => Cow::Borrowed("/var/freight/"),
            Self::CommissionGit { .. } => Cow::Borrowed("/var/git/"),
            Self::PkgCache => Cow::Borrowed("/var/pkgcache/"),
            Self::Context => Cow::Borrowed("/var/context/"),
            Self::ManorHome => Cow::Owned(format!("/home/{MAYOR_UID}/")),
        }
    }

    /// Access mode per the SPEC §4 table.
    #[must_use]
    pub fn mode(&self) -> MountMode {
        match self {
            Self::CommissionGit { .. } | Self::Context => MountMode::Ro,
            Self::AgentHome { .. }
            | Self::Mail
            | Self::Freight
            | Self::PkgCache
            | Self::ManorHome => MountMode::Rw,
        }
    }

    /// Render the `<host>:<container>[:ro]` value for a `podman -v` flag.
    ///
    /// # Errors
    ///
    /// [`MountError::HostPathColon`] if the village root contains a `:` —
    /// podman splits volume values on colons, so such roots cannot be
    /// mounted safely (fail hard, never quote-and-hope; ADR-0010).
    pub fn to_volume_value(&self, tree: &HamletTree) -> Result<String, MountError> {
        let host = self.host_path(tree);
        if host.to_string_lossy().contains(':') {
            return Err(MountError::HostPathColon { path: host });
        }
        let mut value = format!("{}:{}", host.display(), self.container_path());
        if self.mode() == MountMode::Ro {
            value.push_str(":ro");
        }
        Ok(value)
    }
}

/// Why a mount set could not be constructed or rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountError {
    /// `for_agent` was called with a UID outside 1001–2000 (SPEC §4 slot
    /// policy). The Mayor (1000) uses [`MountSet::for_mayor`]; anything
    /// else is not an agent slot.
    AgentUidOutOfRange { uid: u32 },
    /// The village root path contains `:` (see
    /// [`MountSpec::to_volume_value`]).
    HostPathColon { path: PathBuf },
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentUidOutOfRange { uid } => write!(
                f,
                "agent UID {uid} outside slot range {FIRST_AGENT_UID}..={LAST_AGENT_UID}"
            ),
            Self::HostPathColon { path } => {
                write!(
                    f,
                    "host path {} contains ':' — unsafe for podman -v",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for MountError {}

/// The complete, ordered mount set for one container.
///
/// Constructible **only** via [`MountSet::for_agent`] and
/// [`MountSet::for_mayor`] — the exact SPEC §4 rows, no additions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSet {
    specs: Vec<MountSpec>,
}

impl MountSet {
    /// Mount set for a subagent container (SPEC §4: "every agent container
    /// receives exactly these"): parcel home, mail, freight, commission git
    /// (ro), pkgcache, context (ro). Manor home is Mayor-only and absent.
    ///
    /// # Errors
    ///
    /// [`MountError::AgentUidOutOfRange`] unless `uid` is a valid subagent
    /// slot (1001–2000). The caller must pass a charter-allocated UID
    /// ([`crate::slots`]); this guard makes non-slot homes unrepresentable.
    pub fn for_agent(uid: u32, commission: u64) -> Result<Self, MountError> {
        if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(MountError::AgentUidOutOfRange { uid });
        }
        Ok(Self {
            specs: vec![
                MountSpec::AgentHome { uid },
                MountSpec::Mail,
                MountSpec::Freight,
                MountSpec::CommissionGit { commission },
                MountSpec::PkgCache,
                MountSpec::Context,
            ],
        })
    }

    /// Mount set for the Mayor container (UID 1000): manor home plus mail,
    /// freight, pkgcache, context (ro). No commission git mount — the Mayor
    /// is multi-commission and receives git diffs inside §7 context packets,
    /// never a direct repository bind.
    #[must_use]
    pub fn for_mayor() -> Self {
        Self {
            specs: vec![
                MountSpec::ManorHome,
                MountSpec::Mail,
                MountSpec::Freight,
                MountSpec::PkgCache,
                MountSpec::Context,
            ],
        }
    }

    /// The rows of this set, in render order (SPEC §4 table order).
    #[must_use]
    pub fn specs(&self) -> &[MountSpec] {
        &self.specs
    }

    /// Render the full `podman run` volume arguments: `-v <spec>` pairs,
    /// deterministic order, pure strings (ADR-0010 — the daemon passes this
    /// `Vec<String>` straight into `Command::args`, never through a shell).
    ///
    /// # Errors
    ///
    /// Propagates [`MountError::HostPathColon`] from any row.
    pub fn to_podman_args(&self, tree: &HamletTree) -> Result<Vec<String>, MountError> {
        let mut args = Vec::with_capacity(self.specs.len() * 2);
        for spec in &self.specs {
            args.push("-v".to_owned());
            args.push(spec.to_volume_value(tree)?);
        }
        Ok(args)
    }
}

/// Helper for callers asserting isolation invariants: does this set contain
/// any writable mount at the given container path? (QA suite, Sprint 02 §5.)
#[must_use]
pub fn is_writable(set: &MountSet, container_path: &str) -> bool {
    set.specs()
        .iter()
        .any(|s| s.mode() == MountMode::Rw && s.container_path() == Cow::Borrowed(container_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> HamletTree {
        HamletTree::new("/village/.hamlet")
    }

    /// Golden vector: agent mount args for uid 1001, commission 3 —
    /// exactly the SPEC §4 table, in table order, ro where mandated.
    #[test]
    fn agent_args_golden() {
        let set = MountSet::for_agent(1001, 3).expect("1001 is a valid slot");
        let args = set.to_podman_args(&tree()).expect("colon-free root");
        assert_eq!(
            args,
            vec![
                "-v",
                "/village/.hamlet/keep/census/parcel1001/home:/home/1001/",
                "-v",
                "/village/.hamlet/courier/mail:/var/mail/",
                "-v",
                "/village/.hamlet/courier/freight:/var/freight/",
                "-v",
                "/village/.hamlet/guild/ledger/commission3/git:/var/git/:ro",
                "-v",
                "/village/.hamlet/market/pkgcache:/var/pkgcache/",
                "-v",
                "/village/.hamlet/tavern/gossip:/var/context/:ro",
            ]
        );
    }

    /// Golden vector: Mayor mount args — manor home at /home/1000/, no git.
    #[test]
    fn mayor_args_golden() {
        let args = MountSet::for_mayor()
            .to_podman_args(&tree())
            .expect("colon-free root");
        assert_eq!(
            args,
            vec![
                "-v",
                "/village/.hamlet/keep/census/manor/home:/home/1000/",
                "-v",
                "/village/.hamlet/courier/mail:/var/mail/",
                "-v",
                "/village/.hamlet/courier/freight:/var/freight/",
                "-v",
                "/village/.hamlet/market/pkgcache:/var/pkgcache/",
                "-v",
                "/village/.hamlet/tavern/gossip:/var/context/:ro",
            ]
        );
        assert!(
            !args.iter().any(|a| a.contains("/var/git")),
            "Mayor has no git bind"
        );
    }

    #[test]
    fn ro_rows_are_exactly_git_and_context() {
        for set in [MountSet::for_agent(2000, 1).unwrap(), MountSet::for_mayor()] {
            for spec in set.specs() {
                let ro = spec.mode() == MountMode::Ro;
                assert_eq!(
                    ro,
                    matches!(spec, MountSpec::CommissionGit { .. } | MountSpec::Context),
                    "unexpected mode for {spec:?}"
                );
            }
        }
    }

    #[test]
    fn agent_uid_range_enforced() {
        for uid in [0u32, 1, 999, MAYOR_UID, LAST_AGENT_UID + 1, u32::MAX] {
            assert_eq!(
                MountSet::for_agent(uid, 1),
                Err(MountError::AgentUidOutOfRange { uid }),
                "uid {uid} must not produce a mount set"
            );
        }
        for uid in [FIRST_AGENT_UID, 1500, LAST_AGENT_UID] {
            assert!(
                MountSet::for_agent(uid, 1).is_ok(),
                "uid {uid} is a valid slot"
            );
        }
    }

    #[test]
    fn colon_in_root_fails_hard() {
        let tree = HamletTree::new("/tmp/we:ird/.hamlet");
        let set = MountSet::for_agent(1001, 1).unwrap();
        let err = set.to_podman_args(&tree).expect_err("colon root must fail");
        assert!(matches!(err, MountError::HostPathColon { .. }), "{err:?}");
    }

    #[test]
    fn no_arbitrary_mounts_exist() {
        // The only MountSpec shapes are the seven SPEC §4 rows; container
        // paths are fixed literals (or /home/<uid>/). Enumerate exhaustively:
        let all = [
            MountSpec::AgentHome { uid: 1001 },
            MountSpec::Mail,
            MountSpec::Freight,
            MountSpec::CommissionGit { commission: 1 },
            MountSpec::PkgCache,
            MountSpec::Context,
            MountSpec::ManorHome,
        ];
        let paths: Vec<_> = all
            .iter()
            .map(|s| s.container_path().into_owned())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/home/1001/",
                "/var/mail/",
                "/var/freight/",
                "/var/git/",
                "/var/pkgcache/",
                "/var/context/",
                "/home/1000/",
            ]
        );
    }

    #[test]
    fn writable_helper_flags_only_rw_rows() {
        let set = MountSet::for_agent(1001, 3).unwrap();
        assert!(is_writable(&set, "/home/1001/"));
        assert!(is_writable(&set, "/var/mail/"));
        assert!(!is_writable(&set, "/var/git/"), "QA invariant: git is ro");
        assert!(
            !is_writable(&set, "/var/context/"),
            "QA invariant: context is ro"
        );
        assert!(
            !is_writable(&set, "/etc/"),
            "non-table paths are never writable"
        );
    }
}
