//! `hamlet init` bootstrap (SPEC §2 topology, §11.4 secrets).
//!
//! Creates the full normative village tree inside a target working
//! directory (`<dir>/.hamlet/`) via [`HamletTree::ensure`] — the one
//! sanctioned core I/O point (ADR-0003) — then writes the three top-level
//! artifacts:
//!
//! - `config.json` — default [`Config`] (versioned, validated schema), 0644;
//! - `charter.json` — [`Charter::empty`] versioned skeleton, 0644;
//! - `.env` — commented placeholder template ([`ENV_TEMPLATE`]), 0600.
//!
//! All three are written **atomically** (unique same-dir temp → fsync →
//! rename → dir fsync — CONVENTIONS §3, ADR-0006) and **never clobbered**:
//! re-running init on an existing tree is a no-op that reports what exists.
//! Canonical modes are enforced deterministically on EVERY run regardless
//! of umask: static dirs 0755, JSON artifacts 0644, `.env` 0600 (the only
//! secret-bearing file in the tree). Init never follows a symlink and never
//! adopts foreign content: a non-regular artifact path or a `.hamlet/`
//! holding entries outside the normative layout is a typed refusal
//! (ADR-0007).
//!
//! The CLI performs this bootstrap I/O itself; `hamlet-core` stays pure
//! apart from `ensure` (ADR-0001 purity seam).

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use hamlet_core::charter::Charter;
use hamlet_core::config::{Config, ENV_TEMPLATE};
use hamlet_core::tree::HamletTree;

/// Whether init created an artifact or found it already in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitState {
    /// Did not exist (or existed empty, for `.env`); init wrote it.
    Created,
    /// Already present with content; left untouched.
    Existed,
}

impl InitState {
    /// Operator-facing wording for the init report.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Existed => "exists — untouched",
        }
    }
}

/// What one `hamlet init` run did. Deterministic, comparable — the
/// idempotency contract asserts an all-[`InitState::Existed`] second run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    /// The village root (`<target>/.hamlet`).
    pub village_root: std::path::PathBuf,
    /// Static normative directory tree.
    pub tree: InitState,
    /// `config.json`
    pub config: InitState,
    /// `charter.json`
    pub charter: InitState,
    /// `.env` (template written only when absent or empty).
    pub env: InitState,
}

impl InitReport {
    /// Human-readable report lines (artifact order: tree, then top-level
    /// files). The village root is printed by the caller as the header.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!(
                "tree          {} (static normative dirs, 0755)",
                self.tree.as_str()
            ),
            format!(
                "config.json   {} (defaults, config_version {})",
                self.config.as_str(),
                hamlet_core::config::CONFIG_VERSION
            ),
            format!(
                "charter.json  {} (empty v{} skeleton)",
                self.charter.as_str(),
                self.charter_version()
            ),
            format!(".env          {} (template, 0600)", self.env.as_str()),
        ]
    }

    fn charter_version(&self) -> u32 {
        hamlet_core::charter::CHARTER_VERSION
    }
}

/// Bootstrap failure with the failing step named.
#[derive(Debug)]
pub enum InitError {
    /// I/O failure; `context` names the step (never contains secret values).
    Io {
        /// What init was attempting.
        context: String,
        /// Underlying failure.
        source: std::io::Error,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for InitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Bootstrap `Result` alias.
pub type Result<T> = std::result::Result<T, InitError>;

fn io_err(context: &str) -> impl FnOnce(std::io::Error) -> InitError + '_ {
    move |source| InitError::Io {
        context: context.to_string(),
        source,
    }
}

/// Run `hamlet init` against a target working directory. Idempotent: an
/// existing tree is completed (missing static dirs) but existing artifacts
/// are never rewritten or re-templated — only re-normalized to their
/// canonical modes (ADR-0007). Refuses with a typed [`InitError::Io`]
/// (nothing created, nothing followed, foreign content untouched) when the
/// village root is not a real directory, holds foreign content, or any
/// artifact path is occupied by a non-regular file.
///
/// # Errors
///
/// [`InitError::Io`] naming the failing step.
pub fn run(target_dir: &Path) -> Result<InitReport> {
    fs::create_dir_all(target_dir).map_err(io_err("create target working directory"))?;
    let tree = HamletTree::in_working_dir(target_dir);

    // A present village root must hold ONLY the normative layout; foreign
    // content is refused BEFORE anything is created inside it (QA F01-03,
    // ADR-0007). A missing root is the fresh-init path.
    refuse_foreign_content(&tree)?;

    // Capture pre-state BEFORE ensure() materializes anything. lstat-based:
    // a non-regular artifact (symlink/dir/device) is a typed refusal — init
    // never writes or chmods through a planted link (QA F01-02/F01-06).
    let root_existed = tree.root().is_dir();
    let config_state = classify_artifact(&tree.config())?;
    let charter_state = classify_artifact(&tree.charter())?;
    let env_state = classify_artifact(&tree.env_file())?;

    HamletTree::ensure(tree.root()).map_err(io_err("create normative tree"))?;
    normalize_dir_modes(&tree)?;

    match config_state {
        // Canonical modes are enforced on EVERY run (step-02 binding note,
        // QA F01-05); existing content is never touched.
        Artifact::Regular => normalize_file_mode(&tree.config(), 0o644, "config.json")?,
        Artifact::Empty | Artifact::Missing => atomic_write(
            &tree.config(),
            Config::default().render().as_bytes(),
            0o644,
            "write config.json",
        )?,
    }
    match charter_state {
        Artifact::Regular => normalize_file_mode(&tree.charter(), 0o644, "charter.json")?,
        // Charter persistence goes through the core atomic-write protocol
        // (ADR-0004/ADR-0006) so exactly one implementation of
        // temp → fsync → rename exists for charter.json.
        Artifact::Empty | Artifact::Missing => {
            Charter::empty()
                .save_atomic(&tree.charter())
                .map_err(|source| InitError::Io {
                    context: "write charter.json".to_string(),
                    source: std::io::Error::other(source),
                })?;
        }
    }
    match env_state {
        // Existing .env content is preserved verbatim; ensure() already
        // forced its mode to 0600 (ADR-0003/ADR-0007).
        Artifact::Regular => {}
        Artifact::Empty | Artifact::Missing => atomic_write(
            &tree.env_file(),
            ENV_TEMPLATE.as_bytes(),
            0o600,
            "write .env template",
        )?,
    }

    Ok(InitReport {
        village_root: tree.root().to_path_buf(),
        tree: if root_existed {
            InitState::Existed
        } else {
            InitState::Created
        },
        config: config_state.report(),
        charter: charter_state.report(),
        env: env_state.report(),
    })
}

/// What sits at an artifact path, classified via `symlink_metadata` — never
/// through a symlink (ADR-0007). Anything that is not a regular file is a
/// typed refusal: init must never create, chmod, or template through a
/// planted link (QA F01-02 — a mode oracle / out-of-tree write primitive)
/// nor adopt a directory as the secrets file (QA F01-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Artifact {
    /// Path does not exist.
    Missing,
    /// Regular file, zero length (e.g. a bare `.env` from a prior `ensure`).
    Empty,
    /// Regular file with content — never rewritten, only mode-normalized.
    Regular,
}

impl Artifact {
    fn report(self) -> InitState {
        match self {
            Self::Regular => InitState::Existed,
            Self::Empty | Self::Missing => InitState::Created,
        }
    }
}

fn classify_artifact(path: &Path) -> Result<Artifact> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            if meta.len() > 0 {
                Ok(Artifact::Regular)
            } else {
                Ok(Artifact::Empty)
            }
        }
        Ok(_) => Err(InitError::Io {
            context: format!("inspect {}", path.display()),
            source: std::io::Error::other(
                "artifact path is not a regular file (symlink/dir/other); refusing to follow it",
            ),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Artifact::Missing),
        Err(e) => Err(InitError::Io {
            context: format!("stat {}", path.display()),
            source: e,
        }),
    }
}

/// Refuse to adopt a `.hamlet/` root that is not a real directory or that
/// holds entries outside the normative layout (SPEC §2; QA F01-03). Runs
/// before anything is created inside the root; foreign entries are never
/// touched. The allowlist is derived from the typed tree API — no
/// hand-maintained name list.
fn refuse_foreign_content(tree: &HamletTree) -> Result<()> {
    match fs::symlink_metadata(tree.root()) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(InitError::Io {
                context: format!("inspect {}", tree.root().display()),
                source: std::io::Error::other(
                    "village root exists but is not a real directory; refusing to follow or adopt it",
                ),
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(InitError::Io {
                context: format!("stat {}", tree.root().display()),
                source: e,
            });
        }
    }

    let mut allowed: BTreeSet<String> = BTreeSet::new();
    for dir in tree.static_dirs() {
        if let Ok(rel) = dir.strip_prefix(tree.root()) {
            if let Some(first) = rel.components().next() {
                allowed.insert(first.as_os_str().to_string_lossy().into_owned());
            }
        }
    }
    for artifact in [tree.env_file(), tree.config(), tree.charter()] {
        if let Some(name) = artifact.file_name() {
            allowed.insert(name.to_string_lossy().into_owned());
        }
    }

    let mut foreign: Vec<String> = Vec::new();
    let entries = fs::read_dir(tree.root()).map_err(|source| InitError::Io {
        context: format!("inspect {}", tree.root().display()),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| InitError::Io {
            context: format!("inspect {}", tree.root().display()),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !allowed.contains(&name) {
            foreign.push(name);
        }
    }
    if !foreign.is_empty() {
        foreign.sort();
        return Err(InitError::Io {
            context: format!(
                "refuse to adopt {} as a village: foreign content inside the root ({})",
                tree.root().display(),
                foreign.join(", ")
            ),
            source: std::io::Error::other(
                "village root holds entries outside the normative layout (SPEC §2)",
            ),
        });
    }
    Ok(())
}

/// Re-normalize the canonical mode of an EXISTING regular artifact on
/// every run (step-02 binding note; QA F01-05). Content is never touched;
/// the path was verified a regular file by [`classify_artifact`].
#[cfg(unix)]
fn normalize_file_mode(path: &Path, mode: u32, what: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(io_err(&format!("normalize {what} mode")))
}

#[cfg(not(unix))]
fn normalize_file_mode(_path: &Path, _mode: u32, _what: &str) -> Result<()> {
    Ok(())
}

/// Enforce canonical 0755 on every static dir, deterministic under any
/// umask (golden-file contract). Lazy per-identity dirs are daemon-owned
/// and untouched.
#[cfg(unix)]
fn normalize_dir_modes(tree: &HamletTree) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o755);
    for dir in tree.static_dirs() {
        fs::set_permissions(&dir, perms.clone()).map_err(io_err("normalize directory modes"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn normalize_dir_modes(_tree: &HamletTree) -> Result<()> {
    Ok(())
}

/// Atomic file write (CONVENTIONS §3): temp in the same directory → write →
/// fsync → set mode → rename → fsync parent dir. On failure the temp file is
/// removed best-effort and the destination is left untouched.
fn atomic_write(path: &Path, contents: &[u8], mode: u32, context: &str) -> Result<()> {
    atomic_write_inner(path, contents, canonical_perms(mode), context)
}

/// Process-wide serial for unique temp names (ADR-0006).
fn temp_serial() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

#[cfg(unix)]
fn canonical_perms(mode: u32) -> Option<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    Some(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn canonical_perms(_mode: u32) -> Option<fs::Permissions> {
    None
}

fn atomic_write_inner(
    path: &Path,
    contents: &[u8],
    perms: Option<fs::Permissions>,
    context: &str,
) -> Result<()> {
    let parent = path.parent().ok_or_else(|| InitError::Io {
        context: format!("{context}: no parent directory"),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"),
    })?;
    let file_name = path.file_name().ok_or_else(|| InitError::Io {
        context: format!("{context}: no file name"),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"),
    })?;
    // OsStr composition — no lossy string round-trip through the path.
    // Unique per attempt (ADR-0006): concurrent writers never share a temp.
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), temp_serial()));
    let tmp = parent.join(tmp_name);

    let result = (|| -> std::io::Result<()> {
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(contents)?;
            file.sync_all()?;
        }
        if let Some(perms) = perms {
            fs::set_permissions(&tmp, perms)?;
        }
        fs::rename(&tmp, path)?;
        // fsync the directory so the rename itself is durable.
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    result.map_err(|source| {
        let _ = fs::remove_file(&tmp); // best-effort cleanup
        InitError::Io {
            context: context.to_string(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_all_artifacts_then_reports_existed() {
        let tmp = tempfile::tempdir().unwrap();
        let report = run(tmp.path()).unwrap();
        assert_eq!(report.tree, InitState::Created);
        assert_eq!(report.config, InitState::Created);
        assert_eq!(report.charter, InitState::Created);
        assert_eq!(report.env, InitState::Created);
        assert_eq!(report.village_root, tmp.path().join(".hamlet"));

        let second = run(tmp.path()).unwrap();
        assert_eq!(second.tree, InitState::Existed);
        assert_eq!(second.config, InitState::Existed);
        assert_eq!(second.charter, InitState::Existed);
        assert_eq!(second.env, InitState::Existed);
    }

    /// Clobber-guard: operator edits to charter/config/.env survive re-init
    /// byte-for-byte (the artifacts are truth, init is only a bootstrap).
    #[test]
    fn reinit_never_clobbers_modified_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let report = run(tmp.path()).unwrap();
        let edits = [
            (
                report.village_root.join("config.json"),
                b"{\"edited\":1}\n".as_slice(),
            ),
            (
                report.village_root.join("charter.json"),
                b"{\"charter_version\":1,\"edited\":true}\n",
            ),
            (
                report.village_root.join(".env"),
                b"HAMLET_MODEL_API_KEY=test-key-do-not-use\n",
            ),
        ];
        for (path, contents) in &edits {
            fs::write(path, contents).unwrap();
        }
        let second = run(tmp.path()).unwrap();
        assert_eq!(second.config, InitState::Existed);
        assert_eq!(second.charter, InitState::Existed);
        assert_eq!(second.env, InitState::Existed);
        for (path, contents) in &edits {
            assert_eq!(
                fs::read(path).unwrap(),
                contents.to_vec(),
                "{}",
                path.display()
            );
        }
        // .env stays 0600 even after an external write loosened nothing
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(report.village_root.join(".env"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// Init completes a partial tree (root exists, artifacts missing) without
    /// treating it as fresh.
    #[test]
    fn init_completes_partial_tree() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".hamlet")).unwrap();
        let report = run(tmp.path()).unwrap();
        assert_eq!(report.tree, InitState::Existed);
        assert_eq!(report.config, InitState::Created);
        assert!(report.village_root.join("guild/register").is_dir());
    }
}
