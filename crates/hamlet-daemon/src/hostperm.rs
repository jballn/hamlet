//! Host-side permission model (SPEC §4 braces; ADR-0011).
//!
//! The ro mount flags are the *belt*; these host modes/owners are the
//! *braces*: before any container starts, the daemon establishes ownership
//! and modes so the normative mount table is genuinely enforcing even for
//! rw-shared mounts (`/var/mail/`, `/var/freight/`, `/var/pkgcache/`).
//!
//! Per-UID ownership (parcel `home/`, mail spool dirs) is applied via
//! `podman unshare chown <uid>:<gid>` — the daemon's own userns, the same
//! subuid mapping containers see (ADR-0011 cl. 2); the daemon never chowns
//! on the bare host and never needs real root. Modes are plain `chmod`
//! (the daemon owns every path it modes).
//!
//! Creation of the directories themselves belongs to the caller
//! ([`crate::census`] provisioning tracks what it created for rollback);
//! this module only hardens what exists.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hamlet_core::sanitize::sanitize_name;
use hamlet_core::slots::{FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};
use hamlet_core::tree::HamletTree;

use crate::podman::{PodmanError, Runtime};

/// Canonical host modes (ADR-0011 cl. 3 table).
pub const MODE_PARCEL_DIR: u32 = 0o755;
/// Parcel docs (TITLE/DEED/PROPRIETOR): daemon-owned, agent never binds them.
pub const MODE_PARCEL_DOC: u32 = 0o644;
/// Parcel `home/`: private to the slot UID (subuid-owned via unshare).
pub const MODE_PARCEL_HOME: u32 = 0o700;
/// Per-UID mail spool: private to the slot UID; cross-UID reads blocked here
/// (SPEC §4 "spool perms"), since `/var/mail/` is shared by every container.
pub const MODE_MAIL_SPOOL: u32 = 0o700;
/// `courier/mail/` root: traversable so containers can reach their spool.
pub const MODE_MAIL_ROOT: u32 = 0o755;
/// `courier/freight/`: 0710 + ACL-ready — Sprint 04 layers POSIX ACL
/// sandboxes on top (`setfacl` inside containers); nothing here blocks that.
pub const MODE_FREIGHT: u32 = 0o710;
/// Commission `git/` + `tavern/gossip/`: readable by all slots, writable by
/// none (daemon-owned; the `:ro` bind is the belt).
pub const MODE_SHARED_READONLY: u32 = 0o755;
/// `market/pkgcache/`: sticky world-writable shared cache (rw, cache
/// semantics — every slot's toolchain writes here).
pub const MODE_PKGCACHE: u32 = 0o1777;
/// `market/pkgcache/dist/`: the pinned-artifact mirror blueprint bootstrap
/// fills (ADR-0013 cl. 1). Same sticky world-writable mode as its parent so
/// EVERY slot UID can publish a verified artifact into it; the sticky bit is
/// what stops one slot clobbering another's cached bytes.
pub const MODE_PKGCACHE_DIST: u32 = 0o1777;

/// Host-permission failure: a chmod/mode io error or a podman-unshare error.
#[derive(Debug)]
pub enum HostPermError {
    /// Filesystem failure with the step that failed.
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// `podman unshare chown` failed (carries argv + stderr per ADR-0010 cl. 5).
    Podman(PodmanError),
    /// A UID outside the charter slot policy reached the permission layer.
    UidPolicy {
        /// The rejected uid.
        uid: u32,
    },
}

impl fmt::Display for HostPermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "host permissions: {context}: {source}"),
            Self::Podman(e) => write!(f, "host permissions: {e}"),
            Self::UidPolicy { uid } => write!(
                f,
                "host permissions: uid {uid} is neither the Mayor ({MAYOR_UID}) nor a subagent slot ({FIRST_AGENT_UID}..={LAST_AGENT_UID})"
            ),
        }
    }
}

impl std::error::Error for HostPermError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Podman(e) => Some(e),
            Self::UidPolicy { .. } => None,
        }
    }
}

impl From<PodmanError> for HostPermError {
    fn from(e: PodmanError) -> Self {
        Self::Podman(e)
    }
}

fn io_err(context: &str) -> impl FnOnce(std::io::Error) -> HostPermError + '_ {
    move |source| HostPermError::Io {
        context: context.to_string(),
        source,
    }
}

/// Force a canonical mode regardless of umask at creation time.
///
/// # Errors
///
/// [`HostPermError::Io`] naming the path.
pub fn set_mode(path: &Path, mode: u32) -> Result<(), HostPermError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(io_err(&format!("chmod {mode:o} {}", path.display())))
}

/// Applies the ADR-0011 host permission table through the [`Runtime`] seam
/// (real podman or [`crate::podman::FakeRuntime`] — chown argv is recorded
/// and golden-tested like every other podman invocation).
pub struct HostPermissions<'a> {
    runtime: &'a (dyn Runtime + 'a),
}

impl<'a> HostPermissions<'a> {
    /// Borrow a runtime (the daemon holds `Arc<dyn Runtime>`).
    #[must_use]
    pub fn new(runtime: &'a (dyn Runtime + 'a)) -> Self {
        Self { runtime }
    }

    /// Parcel `home/`: 0700 + owned by the slot uid (via `podman unshare
    /// chown`). The home must already exist ([`crate::census`] creates and
    /// tracks it for rollback).
    ///
    /// # Errors
    ///
    /// [`HostPermError`] on a non-slot uid, chmod failure, or unshare failure.
    pub async fn apply_parcel_home(
        &self,
        tree: &HamletTree,
        uid: u32,
    ) -> Result<(), HostPermError> {
        if !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(HostPermError::UidPolicy { uid });
        }
        let home = tree.parcel_home(uid);
        set_mode(&home, MODE_PARCEL_HOME)?;
        self.chown(uid, &[home]).await
    }

    /// Mail spool `courier/mail/<uid>/`: 0700 + owned by the slot uid, and
    /// the spool root 0755 so containers can traverse to it. The daemon
    /// writes spools from inside its userns (ADR-0011 cl. 4). Mayor spool
    /// (uid 1000) is valid.
    ///
    /// # Errors
    ///
    /// [`HostPermError`] on a non-slot uid, chmod failure, or unshare failure.
    pub async fn apply_mail_spool(&self, tree: &HamletTree, uid: u32) -> Result<(), HostPermError> {
        if uid != MAYOR_UID && !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(HostPermError::UidPolicy { uid });
        }
        set_mode(&tree.courier_mail(), MODE_MAIL_ROOT)?;
        let name = sanitize_name(&uid.to_string()).map_err(|e| HostPermError::Io {
            context: format!("spool name for uid {uid}: {e}"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "uid name"),
        })?;
        let spool = tree.courier_mail_recipient(&name);
        set_mode(&spool, MODE_MAIL_SPOOL)?;
        self.chown(uid, &[spool]).await
    }

    /// Shared-mount hardening for one commission (ADR-0011 cl. 3):
    /// commission `git/` 0755, `tavern/gossip/` 0755 (both daemon-owned:
    /// readable by all slots, writable by none), `courier/freight/` 0710
    /// (ACL-ready), `market/pkgcache/` 01777 (sticky shared cache).
    ///
    /// # Errors
    ///
    /// [`HostPermError::Io`] on any chmod failure.
    pub async fn apply_shared(
        &self,
        tree: &HamletTree,
        commission: u64,
    ) -> Result<(), HostPermError> {
        set_mode(&tree.commission_git(commission), MODE_SHARED_READONLY)?;
        set_mode(&tree.tavern_gossip(), MODE_SHARED_READONLY)?;
        set_mode(&tree.courier_freight(), MODE_FREIGHT)?;
        set_mode(&tree.market_pkgcache(), MODE_PKGCACHE)?;
        Ok(())
    }

    async fn chown(&self, uid: u32, paths: &[std::path::PathBuf]) -> Result<(), HostPermError> {
        self.runtime
            .unshare_chown(uid, uid, paths)
            .await
            .map_err(HostPermError::Podman)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::podman::FakeRuntime;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn village() -> (tempfile::TempDir, HamletTree) {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        (tmp, tree)
    }

    #[tokio::test]
    async fn parcel_home_is_0700_and_chowned_via_unshare() {
        let (_tmp, tree) = village();
        std::fs::create_dir_all(tree.parcel_home(1001)).unwrap();
        let fake = Arc::new(FakeRuntime::new());
        let perms = HostPermissions::new(fake.as_ref());
        perms.apply_parcel_home(&tree, 1001).await.unwrap();
        assert_eq!(mode_of(&tree.parcel_home(1001)), 0o700);
        assert_eq!(
            fake.argv_log().pop().unwrap(),
            vec![
                "podman",
                "unshare",
                "chown",
                "1001:1001",
                "--",
                tree.parcel_home(1001).to_str().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn mail_spool_is_0700_chowned_and_root_traversable() {
        let (_tmp, tree) = village();
        let spool = tree.courier_mail_recipient(&sanitize_name("1042").unwrap());
        std::fs::create_dir_all(&spool).unwrap();
        let fake = Arc::new(FakeRuntime::new());
        let perms = HostPermissions::new(fake.as_ref());
        perms.apply_mail_spool(&tree, 1042).await.unwrap();
        assert_eq!(mode_of(&spool), 0o700);
        assert_eq!(mode_of(&tree.courier_mail()), 0o755);
        let argv = fake.argv_log().pop().unwrap();
        assert_eq!(argv[1..5], ["unshare", "chown", "1042:1042", "--"]);
        assert_eq!(argv[5], spool.to_str().unwrap());
    }

    #[tokio::test]
    async fn shared_mounts_get_the_adr_0011_modes() {
        let (_tmp, tree) = village();
        std::fs::create_dir_all(tree.commission_git(3)).unwrap();
        let fake = FakeRuntime::new();
        let perms = HostPermissions::new(&fake);
        perms.apply_shared(&tree, 3).await.unwrap();
        assert_eq!(mode_of(&tree.commission_git(3)), 0o755);
        assert_eq!(mode_of(&tree.tavern_gossip()), 0o755);
        assert_eq!(mode_of(&tree.courier_freight()), 0o710);
        assert_eq!(mode_of(&tree.market_pkgcache()), 0o1777);
        assert!(fake.argv_log().is_empty(), "chmod never spawns podman");
    }

    #[tokio::test]
    async fn uid_policy_is_enforced_before_any_side_effect() {
        let (_tmp, tree) = village();
        let fake = FakeRuntime::new();
        let perms = HostPermissions::new(&fake);
        for uid in [0u32, 999, 2001, u32::MAX] {
            assert!(matches!(
                perms.apply_parcel_home(&tree, uid).await,
                Err(HostPermError::UidPolicy { uid: u }) if u == uid
            ));
            assert!(matches!(
                perms.apply_mail_spool(&tree, uid).await,
                Err(HostPermError::UidPolicy { .. })
            ));
        }
        // Nothing was chmod'd or spawned for rejected uids.
        assert!(fake.argv_log().is_empty());
        // The Mayor slot IS valid for spools (manor mail) but never for
        // parcel homes.
        let mayor_spool = tree.courier_mail_recipient(&sanitize_name("1000").unwrap());
        std::fs::create_dir_all(&mayor_spool).unwrap();
        perms.apply_mail_spool(&tree, MAYOR_UID).await.unwrap();
        assert_eq!(mode_of(&mayor_spool), 0o700);
        assert!(matches!(
            perms.apply_parcel_home(&tree, MAYOR_UID).await,
            Err(HostPermError::UidPolicy { uid: MAYOR_UID })
        ));
    }

    #[tokio::test]
    async fn chown_failure_surfaces_as_podman_error() {
        let (_tmp, tree) = village();
        std::fs::create_dir_all(tree.parcel_home(1001)).unwrap();
        let fake = FakeRuntime::new();
        fake.fail_next_for(
            "unshare",
            crate::podman::PodmanErrorKind::NonZeroExit { code: Some(1) },
        );
        let perms = HostPermissions::new(&fake);
        let err = perms.apply_parcel_home(&tree, 1001).await.unwrap_err();
        assert!(matches!(err, HostPermError::Podman(_)), "{err}");
    }

    /// `set_mode` failures are typed and name the path.
    #[test]
    fn set_mode_reports_missing_paths() {
        let err = set_mode(&PathBuf::from("/nonexistent/hamlet/test"), 0o700).unwrap_err();
        assert!(
            err.to_string().contains("/nonexistent/hamlet/test"),
            "{err}"
        );
    }
}
