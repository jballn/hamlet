//! Git push mediation schema (SPEC §14, ADR-0037).
//!
//! Agents never write shared refs. The only repo mount an agent holds is rw
//! (`commission<n>/git`), and the mediation contract keeps that true in
//! practice: an agent stages a **git bundle** into its home spool
//! (`/home/<uid>/git-spool/`, host path `tree::parcel_git_spool(uid)`), and
//! the daemon is the only process that validates, applies, or refuses it.
//!
//! This module is the frozen schema step 04 builds against: thresholds, ref
//! naming, identity derivation, candidate-name validation, and the typed
//! rejection vocabulary. The executor (bundle verification, object graft,
//! ref update, archive/consume, `GitDiffReady` wake) is daemon-side.

/// Maximum size of one bundle accepted from a spool (ADR-0037 cl. 2).
pub const MAX_BUNDLE_BYTES: u64 = 32 * 1024 * 1024;

/// The suffix that makes a spool file a push candidate (ADR-0037 cl. 1).
/// Files not ending in this suffix are ignored, never touched.
pub const BUNDLE_SUFFIX: &str = ".bundle";

/// Maximum byte size of the unified-diff body carried by `GitDiffReady`
/// (ADR-0037 cl. 7); above it the body degrades to the deterministic
/// `--stat` summary of the same range — never a truncated diff.
pub const MAX_DIFF_BODY_BYTES: u64 = 64 * 1024;

/// The per-agent branch a slot may advance (ADR-0037 cl. 5): `agent/<uid>`.
#[must_use]
pub fn agent_branch(uid: u32) -> String {
    format!("agent/{uid}")
}

/// A topic-qualified per-agent branch (`agent/<uid>/<topic>`) a slot may
/// advance within its own branch namespace (ADR-0037 cl. 3/5).
#[must_use]
pub fn agent_topic_branch(uid: u32, topic: &crate::sanitize::SafeName) -> String {
    format!("agent/{uid}/{topic}")
}

/// Full ref form used by bundle validation (ADR-0037 cl. 3).
#[must_use]
pub fn agent_branch_ref(uid: u32) -> String {
    format!("refs/heads/agent/{uid}")
}

/// Full ref form for a topic-qualified per-agent branch (`refs/heads/agent/<uid>/<topic>`).
#[must_use]
pub fn agent_topic_branch_ref(uid: u32, topic: &crate::sanitize::SafeName) -> String {
    format!("refs/heads/agent/{uid}/{topic}")
}

/// Whether `reference` is a branch ref owned by `uid`: either the canonical
/// `refs/heads/agent/<uid>` (ADR-0037 cl. 3) or a single-segment topic branch
/// `refs/heads/agent/<uid>/<topic>` where `<topic>` is a valid [`crate::sanitize::SafeName`].
#[must_use]
pub fn is_own_agent_branch_ref(uid: u32, reference: &str) -> bool {
    let base = agent_branch_ref(uid);
    if reference == base {
        return true;
    }
    let Some(rest) = reference.strip_prefix(&base) else {
        return false;
    };
    let Some(topic) = rest.strip_prefix('/') else {
        return false;
    };
    !topic.is_empty() && !topic.contains('/') && crate::sanitize::sanitize_name(topic).is_ok()
}

/// The git identity the daemon seeds at provisioning; every commit in a
/// bundle must carry this as committer **and** author email (ADR-0037 cl. 3).
#[must_use]
pub fn agent_identity_email(uid: u32) -> String {
    format!("{uid}@hamlet.local")
}

/// Typed rejection reasons for a refused bundle (ADR-0037 cl. 3/6). Every
/// refusal is one of these — never a shell-exit mystery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleRejection {
    /// File exceeded [`MAX_BUNDLE_BYTES`].
    Oversize,
    /// `git bundle verify` failed (structurally invalid).
    VerifyFailed,
    /// The bundle carried a ref other than the slot's own branch.
    NotOwnBranch,
    /// A commit lacked the slot's seeded identity.
    ForeignIdentity,
    /// The bundle did not extend the current branch tip (first push: `main`).
    NonFastForward,
}

impl BundleRejection {
    /// Stable token for refusal receipts (`<name>.bundle.<token>`), mail, and
    /// traces (ADR-0037 cl. 6).
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Oversize => "oversize",
            Self::VerifyFailed => "verify",
            Self::NotOwnBranch => "ref",
            Self::ForeignIdentity => "identity",
            Self::NonFastForward => "nonff",
        }
    }
}

impl std::fmt::Display for BundleRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversize => write!(f, "bundle exceeds the {MAX_BUNDLE_BYTES}-byte cap"),
            Self::VerifyFailed => write!(f, "bundle failed `git bundle verify`"),
            Self::NotOwnBranch => write!(f, "bundle does not carry only the slot's own branch"),
            Self::ForeignIdentity => write!(f, "commit lacks the slot's seeded git identity"),
            Self::NonFastForward => write!(f, "bundle is not a fast-forward of the current tip"),
        }
    }
}

/// Whether a spool file name is a bundle candidate (ADR-0037 cl. 1):
/// `^[A-Za-z0-9._-]{1,128}\.bundle$`. Anything else in the spool is ignored —
/// the spool sits in the agent's own home, and not everything there is a push.
#[must_use]
pub fn is_bundle_name(name: &str) -> bool {
    let Some(base) = name.strip_suffix(BUNDLE_SUFFIX) else {
        return false;
    };
    (1..=128).contains(&base.len())
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_and_identity_naming() {
        assert_eq!(agent_branch(1042), "agent/1042");
        assert_eq!(agent_branch_ref(1042), "refs/heads/agent/1042");
        assert_eq!(agent_identity_email(1042), "1042@hamlet.local");
        let topic = crate::sanitize::sanitize_name("feat-x").unwrap();
        assert_eq!(agent_topic_branch(1042, &topic), "agent/1042/feat-x");
        assert_eq!(
            agent_topic_branch_ref(1042, &topic),
            "refs/heads/agent/1042/feat-x"
        );
        assert!(is_own_agent_branch_ref(1042, "refs/heads/agent/1042"));
        assert!(is_own_agent_branch_ref(
            1042,
            "refs/heads/agent/1042/feat-x"
        ));
        assert!(!is_own_agent_branch_ref(1042, "refs/heads/main"));
        assert!(!is_own_agent_branch_ref(1042, "refs/heads/agent/1043"));
        assert!(!is_own_agent_branch_ref(1042, "refs/heads/agent/10420"));
        assert!(!is_own_agent_branch_ref(1042, "refs/heads/agent/1042/"));
        assert!(!is_own_agent_branch_ref(1042, "refs/heads/agent/1042/a/b"));
        assert!(!is_own_agent_branch_ref(1042, "refs/heads/agent/1042/.."));
        assert!(!is_own_agent_branch_ref(1042, "refs/tags/agent/1042"));
    }

    #[test]
    fn bundle_name_validation() {
        assert!(is_bundle_name("push.bundle"));
        assert!(is_bundle_name("a.b_c-9.bundle"));
        // No suffix, wrong suffix.
        assert!(!is_bundle_name("push.tar"));
        assert!(!is_bundle_name("push.bundle2"));
        // Empty base.
        assert!(!is_bundle_name(".bundle"));
        // Overlong base (129 chars).
        let long = "a".repeat(129);
        assert!(!is_bundle_name(&format!("{long}.bundle")));
        // 128-char base is the boundary.
        let edge = "a".repeat(128);
        assert!(is_bundle_name(&format!("{edge}.bundle")));
    }

    #[test]
    fn rejection_tokens_are_stable() {
        assert_eq!(BundleRejection::Oversize.token(), "oversize");
        assert_eq!(BundleRejection::VerifyFailed.token(), "verify");
        assert_eq!(BundleRejection::NotOwnBranch.token(), "ref");
        assert_eq!(BundleRejection::ForeignIdentity.token(), "identity");
        assert_eq!(BundleRejection::NonFastForward.token(), "nonff");
    }

    #[test]
    fn thresholds_are_documented_constants() {
        assert_eq!(MAX_BUNDLE_BYTES, 32 * 1024 * 1024);
        assert_eq!(MAX_DIFF_BODY_BYTES, 64 * 1024);
    }
}
