//! `hamlet vault {list|export}` — CLI surface for inspecting and exporting
//! promoted deliverables from `keep/vault/` (SPEC §7, §9.4, §12.1; ADR-0038).
//!
//! Output is deterministic and derived strictly from the village tree.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use hamlet_core::decree::content_digest;
use hamlet_core::sanitize::{sanitize_name, SanitizeError};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::ledger::{Ledger, LedgerError};

/// Default file permissions for exported artifacts (`0644`).
const MODE_EXPORT_FILE: u32 = 0o644;

/// Errors returned by `hamlet vault` subcommands.
#[derive(Debug)]
pub enum VaultCliError {
    /// Bad CLI invocation (missing required flag/positional, unknown subcommand).
    Usage(String),
    /// A path in the vault or destination is a symlink (refused under ADR-0007).
    SymlinkRefused {
        /// Offending path.
        path: PathBuf,
    },
    /// The specified artifact was not found in the vault.
    ArtifactNotFound {
        /// The artifact name requested.
        artifact: String,
        /// The commission scope if one was specified.
        commission: Option<String>,
    },
    /// Multiple commissions contain an artifact with the same name.
    AmbiguousArtifact {
        /// The artifact name requested.
        artifact: String,
        /// Matching commission slugs.
        matches: Vec<String>,
    },
    /// Slug failed sanitization.
    Sanitize(SanitizeError),
    /// Ledger or village error.
    Ledger(LedgerError),
    /// I/O error during export.
    Io {
        /// Operation and path context.
        context: String,
        /// Underlying OS error.
        source: io::Error,
    },
}

impl fmt::Display for VaultCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg) => write!(f, "{msg}"),
            Self::SymlinkRefused { path } => {
                write!(f, "refusing symlink at {} (ADR-0007)", path.display())
            }
            Self::ArtifactNotFound {
                artifact,
                commission: Some(comm),
            } => {
                write!(
                    f,
                    "artifact '{artifact}' not found in vault for commission '{comm}'"
                )
            }
            Self::ArtifactNotFound {
                artifact,
                commission: None,
            } => {
                write!(f, "artifact '{artifact}' not found in any vault commission")
            }
            Self::AmbiguousArtifact { artifact, matches } => {
                write!(
                    f,
                    "artifact '{artifact}' is ambiguous, found in multiple commissions: {}; use --commission <slug> to disambiguate",
                    matches.join(", ")
                )
            }
            Self::Sanitize(e) => write!(f, "invalid name: {e}"),
            Self::Ledger(e) => write!(f, "{e}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for VaultCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sanitize(e) => Some(e),
            Self::Ledger(e) => Some(e),
            Self::Io { source, .. } => Some(source),
            Self::Usage(_)
            | Self::SymlinkRefused { .. }
            | Self::ArtifactNotFound { .. }
            | Self::AmbiguousArtifact { .. } => None,
        }
    }
}

impl From<SanitizeError> for VaultCliError {
    fn from(e: SanitizeError) -> Self {
        Self::Sanitize(e)
    }
}

impl From<LedgerError> for VaultCliError {
    fn from(e: LedgerError) -> Self {
        Self::Ledger(e)
    }
}

/// Parsed arguments for `hamlet vault list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListArgs {
    /// Optional `--commission <slug>` filter.
    pub commission: Option<String>,
    /// Optional `--dir <working_dir>` override (defaults to `cwd`).
    pub working_dir: Option<PathBuf>,
}

/// Parsed arguments for `hamlet vault export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportArgs {
    /// Artifact file name to export.
    pub artifact: String,
    /// Destination file or directory path.
    pub dest: PathBuf,
    /// Optional `--commission <slug>` filter.
    pub commission: Option<String>,
    /// Optional `--dir <working_dir>` override (defaults to `cwd`).
    pub working_dir: Option<PathBuf>,
}

/// Parsed `hamlet vault` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultCommand {
    /// `hamlet vault list [--commission <slug>]`
    List(ListArgs),
    /// `hamlet vault export <artifact> <dest> [--commission <slug>]`
    Export(ExportArgs),
}

/// Parse argv slice following `hamlet vault`.
///
/// # Errors
///
/// Returns [`VaultCliError::Usage`] when the subcommand or flags are invalid.
pub fn parse_args(args: &[&str]) -> Result<VaultCommand, VaultCliError> {
    let Some((&sub, rest)) = args.split_first() else {
        return Err(VaultCliError::Usage(
            "missing vault subcommand (expected: list | export)".to_owned(),
        ));
    };
    match sub {
        "list" => parse_list_args(rest).map(VaultCommand::List),
        "export" => parse_export_args(rest).map(VaultCommand::Export),
        other => Err(VaultCliError::Usage(format!(
            "unknown vault subcommand '{other}' (expected: list | export)"
        ))),
    }
}

fn parse_list_args(args: &[&str]) -> Result<ListArgs, VaultCliError> {
    let mut commission: Option<String> = None;
    let mut working_dir: Option<PathBuf> = None;
    let mut positional_only = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if !positional_only {
            if arg == "--" {
                positional_only = true;
                i += 1;
                continue;
            }
            if arg == "-h" || arg == "--help" {
                return Err(VaultCliError::Usage(
                    "usage: hamlet vault list [--commission <slug>] [--dir <dir>]".to_owned(),
                ));
            }
            if arg == "--commission" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(VaultCliError::Usage(
                        "--commission requires a slug".to_owned(),
                    ));
                };
                if commission.is_some() {
                    return Err(VaultCliError::Usage(
                        "--commission specified more than once".to_owned(),
                    ));
                }
                commission = Some(val.to_owned());
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--commission=") {
                if val.is_empty() {
                    return Err(VaultCliError::Usage(
                        "--commission requires a slug".to_owned(),
                    ));
                }
                if commission.is_some() {
                    return Err(VaultCliError::Usage(
                        "--commission specified more than once".to_owned(),
                    ));
                }
                commission = Some(val.to_owned());
                i += 1;
                continue;
            }
            if arg == "--dir" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(VaultCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                };
                if working_dir.is_some() {
                    return Err(VaultCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--dir=") {
                if val.is_empty() {
                    return Err(VaultCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                }
                if working_dir.is_some() {
                    return Err(VaultCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
        }
        return Err(VaultCliError::Usage(format!(
            "unexpected argument '{arg}' (usage: hamlet vault list [--commission <slug>])"
        )));
    }
    Ok(ListArgs {
        commission,
        working_dir,
    })
}

fn parse_export_args(args: &[&str]) -> Result<ExportArgs, VaultCliError> {
    let mut artifact: Option<String> = None;
    let mut dest: Option<PathBuf> = None;
    let mut commission: Option<String> = None;
    let mut working_dir: Option<PathBuf> = None;
    let mut positional_only = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if !positional_only {
            if arg == "--" {
                positional_only = true;
                i += 1;
                continue;
            }
            if arg == "-h" || arg == "--help" {
                return Err(VaultCliError::Usage(
                    "usage: hamlet vault export <artifact> <dest> [--commission <slug>] [--dir <dir>]"
                        .to_owned(),
                ));
            }
            if arg == "--commission" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(VaultCliError::Usage(
                        "--commission requires a slug".to_owned(),
                    ));
                };
                if commission.is_some() {
                    return Err(VaultCliError::Usage(
                        "--commission specified more than once".to_owned(),
                    ));
                }
                commission = Some(val.to_owned());
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--commission=") {
                if val.is_empty() {
                    return Err(VaultCliError::Usage(
                        "--commission requires a slug".to_owned(),
                    ));
                }
                if commission.is_some() {
                    return Err(VaultCliError::Usage(
                        "--commission specified more than once".to_owned(),
                    ));
                }
                commission = Some(val.to_owned());
                i += 1;
                continue;
            }
            if arg == "--dir" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(VaultCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                };
                if working_dir.is_some() {
                    return Err(VaultCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--dir=") {
                if val.is_empty() {
                    return Err(VaultCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                }
                if working_dir.is_some() {
                    return Err(VaultCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
        }
        if artifact.is_none() {
            artifact = Some(arg.to_owned());
            i += 1;
        } else if dest.is_none() {
            dest = Some(PathBuf::from(arg));
            i += 1;
        } else {
            return Err(VaultCliError::Usage(format!(
                "unexpected argument '{arg}' (usage: hamlet vault export <artifact> <dest> [--commission <slug>])"
            )));
        }
    }

    let Some(artifact) = artifact else {
        return Err(VaultCliError::Usage(
            "missing <artifact> (usage: hamlet vault export <artifact> <dest> [--commission <slug>])"
                .to_owned(),
        ));
    };
    let Some(dest) = dest else {
        return Err(VaultCliError::Usage(
            "missing <dest> (usage: hamlet vault export <artifact> <dest> [--commission <slug>])"
                .to_owned(),
        ));
    };

    Ok(ExportArgs {
        artifact,
        dest,
        commission,
        working_dir,
    })
}

/// One promoted artifact entry in `keep/vault/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultArtifact {
    /// Commission slug owning the artifact.
    pub commission: String,
    /// Artifact file name.
    pub artifact: String,
    /// Size in bytes.
    pub size: u64,
    /// Recorded sha256 checksum (hex).
    pub sha256: String,
}

/// List vault artifacts matching `filter_commission`.
///
/// # Errors
///
/// Returns [`VaultCliError`] on symlink refusal, invalid slug, or I/O failure.
pub fn run_list(
    working_dir: &Path,
    filter_commission: Option<&str>,
) -> Result<Vec<VaultArtifact>, VaultCliError> {
    let tree = HamletTree::new(working_dir.join(".hamlet"));
    let vault_root = tree.keep_vault();
    if !vault_root.exists() {
        return Ok(Vec::new());
    }

    // Verify village initialized
    let ledger = Ledger::new(tree.clone());
    let _ = ledger.list_commissions()?;

    let mut results = Vec::new();

    if let Some(slug) = filter_commission {
        let safe_slug = sanitize_name(slug)?;
        let comm_dir = tree.keep_vault_commission(&safe_slug);
        if comm_dir.is_dir() {
            scan_commission_vault(&comm_dir, safe_slug.as_str(), &mut results)?;
        }
    } else {
        let mut entries: Vec<_> = fs::read_dir(&vault_root)
            .map_err(|e| VaultCliError::Io {
                context: format!("read dir {}", vault_root.display()),
                source: e,
            })?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                return Err(VaultCliError::SymlinkRefused { path: entry.path() });
            }
            if meta.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if !name.starts_with('.') {
                        scan_commission_vault(&entry.path(), name, &mut results)?;
                    }
                }
            }
        }
    }

    results.sort_by(|a, b| (&a.commission, &a.artifact).cmp(&(&b.commission, &b.artifact)));
    Ok(results)
}

fn scan_commission_vault(
    comm_dir: &Path,
    commission_slug: &str,
    out: &mut Vec<VaultArtifact>,
) -> Result<(), VaultCliError> {
    let entries = fs::read_dir(comm_dir).map_err(|e| VaultCliError::Io {
        context: format!("read dir {}", comm_dir.display()),
        source: e,
    })?;

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            return Err(VaultCliError::SymlinkRefused { path });
        }
        if !meta.is_file() {
            continue;
        }
        let file_name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Skip sidecar files
        if file_name.ends_with(".sha256") || file_name.starts_with('.') {
            continue;
        }

        // Read sidecar checksum if available
        let sidecar_path = comm_dir.join(format!("{file_name}.sha256"));
        let sha256 = if let Ok(sidecar_text) = fs::read_to_string(&sidecar_path) {
            sidecar_text
                .split_whitespace()
                .next()
                .unwrap_or("-")
                .to_owned()
        } else if let Ok(bytes) = fs::read(&path) {
            content_digest(&bytes)
        } else {
            "-".to_owned()
        };

        out.push(VaultArtifact {
            commission: commission_slug.to_owned(),
            artifact: file_name,
            size: meta.len(),
            sha256,
        });
    }

    Ok(())
}

/// Render a slice of [`VaultArtifact`] into a deterministic table.
#[must_use]
pub fn render_vault_list(items: &[VaultArtifact]) -> String {
    let headers = ["COMMISSION", "ARTIFACT", "SIZE", "SHA256"];
    if items.is_empty() {
        return format!(
            "{}  {}  {}  {}\n(no vault artifacts)\n",
            headers[0], headers[1], headers[2], headers[3]
        );
    }

    let rows: Vec<[String; 4]> = items
        .iter()
        .map(|item| {
            [
                item.commission.clone(),
                item.artifact.clone(),
                item.size.to_string(),
                item.sha256.clone(),
            ]
        })
        .collect();

    let mut widths = [0usize; 4];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    write_padded_row(&mut out, &headers.map(str::to_owned), &widths);
    for row in &rows {
        write_padded_row(&mut out, row, &widths);
    }
    out
}

fn write_padded_row(out: &mut String, cells: &[String; 4], widths: &[usize; 4]) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if i < cells.len() - 1 {
            let pad = widths[i].saturating_sub(cell.len());
            for _ in 0..pad {
                out.push(' ');
            }
        }
    }
    out.push('\n');
}

/// Result report from an export operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// Artifact file name.
    pub artifact: String,
    /// Commission slug artifact was exported from.
    pub commission: String,
    /// Path written to.
    pub dest_path: PathBuf,
    /// Total bytes exported.
    pub bytes: u64,
    /// SHA256 checksum.
    pub sha256: String,
}

impl ExportReport {
    /// Human-readable deterministic summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "exported {} from commission '{}' to {} ({} bytes, sha256: {})\n",
            self.artifact,
            self.commission,
            self.dest_path.display(),
            self.bytes,
            self.sha256
        )
    }
}

/// Export an artifact from `keep/vault/` to `dest`.
///
/// # Errors
///
/// Returns [`VaultCliError`] on not found, ambiguity, symlink refusal, or I/O failure.
pub fn run_export(
    working_dir: &Path,
    artifact_name: &str,
    dest: &Path,
    commission_filter: Option<&str>,
) -> Result<ExportReport, VaultCliError> {
    let tree = HamletTree::new(working_dir.join(".hamlet"));
    let vault_root = tree.keep_vault();
    if !vault_root.exists() {
        return Err(VaultCliError::ArtifactNotFound {
            artifact: artifact_name.to_owned(),
            commission: commission_filter.map(ToOwned::to_owned),
        });
    }

    // Refuse symlinks on destination if it exists
    if let Ok(meta) = fs::symlink_metadata(dest) {
        if meta.file_type().is_symlink() {
            return Err(VaultCliError::SymlinkRefused {
                path: dest.to_path_buf(),
            });
        }
    }

    // Identify candidate artifact in vault
    let (source_path, matched_commission) = if let Some(slug) = commission_filter {
        let safe_slug = sanitize_name(slug)?;
        let candidate = tree.vault_commission_artifact(&safe_slug, &sanitize_name(artifact_name)?);
        if let Ok(meta) = fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() {
                return Err(VaultCliError::SymlinkRefused { path: candidate });
            }
            if meta.is_file() {
                (candidate, safe_slug.as_str().to_owned())
            } else {
                return Err(VaultCliError::ArtifactNotFound {
                    artifact: artifact_name.to_owned(),
                    commission: Some(slug.to_owned()),
                });
            }
        } else {
            return Err(VaultCliError::ArtifactNotFound {
                artifact: artifact_name.to_owned(),
                commission: Some(slug.to_owned()),
            });
        }
    } else {
        // Search across all commissions
        let mut matches = Vec::new();
        if let Ok(dir) = fs::read_dir(&vault_root) {
            for entry in dir.filter_map(Result::ok) {
                if let Ok(meta) = fs::symlink_metadata(entry.path()) {
                    if meta.is_dir() && !meta.file_type().is_symlink() {
                        let comm_slug = entry.file_name().to_string_lossy().to_string();
                        let candidate = entry.path().join(artifact_name);
                        if let Ok(cmeta) = fs::symlink_metadata(&candidate) {
                            if cmeta.file_type().is_symlink() {
                                return Err(VaultCliError::SymlinkRefused { path: candidate });
                            }
                            if cmeta.is_file() {
                                matches.push((candidate, comm_slug));
                            }
                        }
                    }
                }
            }
        }
        if matches.is_empty() {
            return Err(VaultCliError::ArtifactNotFound {
                artifact: artifact_name.to_owned(),
                commission: None,
            });
        }
        if matches.len() > 1 {
            let match_slugs: Vec<String> = matches.into_iter().map(|(_, s)| s).collect();
            return Err(VaultCliError::AmbiguousArtifact {
                artifact: artifact_name.to_owned(),
                matches: match_slugs,
            });
        }
        matches.remove(0)
    };

    let bytes = fs::read(&source_path).map_err(|e| VaultCliError::Io {
        context: format!("read vault artifact {}", source_path.display()),
        source: e,
    })?;
    let sha256 = content_digest(&bytes);

    // Resolve target path
    let target_path = if dest.is_dir() {
        dest.join(artifact_name)
    } else {
        dest.to_path_buf()
    };

    // Refuse symlink on target path
    if let Ok(meta) = fs::symlink_metadata(&target_path) {
        if meta.file_type().is_symlink() {
            return Err(VaultCliError::SymlinkRefused {
                path: target_path.clone(),
            });
        }
    }

    // Atomic write to target
    write_atomic_export(&target_path, &bytes, MODE_EXPORT_FILE).map_err(|e| VaultCliError::Io {
        context: format!("write export file {}", target_path.display()),
        source: e,
    })?;

    Ok(ExportReport {
        artifact: artifact_name.to_owned(),
        commission: matched_commission,
        dest_path: target_path,
        bytes: bytes.len() as u64,
        sha256,
    })
}

const TEMP_ATTEMPTS: u32 = 256;

fn write_atomic_export(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;

    for attempt in 0..TEMP_ATTEMPTS {
        let tmp_name = if attempt == 0 {
            format!(".{file_name}.tmp-{}", std::process::id())
        } else {
            format!(".{file_name}.tmp-{}-{}", std::process::id(), attempt)
        };
        let tmp_path = parent.join(tmp_name);

        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };

        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);

        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
        return fs::rename(&tmp_path, path);
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "no free temp name for {} after {TEMP_ATTEMPTS} attempts (all pre-planted)",
            path.display()
        ),
    ))
}

/// Execute a parsed [`VaultCommand`] using `default_working_dir` when
/// `--dir` was not provided, returning the rendered stdout string.
///
/// # Errors
///
/// Returns [`VaultCliError`] on usage, I/O, or ledger failure.
pub fn execute(cmd: &VaultCommand, default_working_dir: &Path) -> Result<String, VaultCliError> {
    match cmd {
        VaultCommand::List(args) => {
            let dir = args.working_dir.as_deref().unwrap_or(default_working_dir);
            let items = run_list(dir, args.commission.as_deref())?;
            Ok(render_vault_list(&items))
        }
        VaultCommand::Export(args) => {
            let dir = args.working_dir.as_deref().unwrap_or(default_working_dir);
            let report = run_export(dir, &args.artifact, &args.dest, args.commission.as_deref())?;
            Ok(report.summary())
        }
    }
}
