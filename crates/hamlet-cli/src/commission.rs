//! `hamlet commission {new|list|show}` — thin CLI surface over
//! [`hamlet_daemon::ledger::Ledger`] (SPEC §9.1, §12.1; ADR-0001, ADR-0036).
//!
//! No orchestration state lives in `hamlet-cli`: every command resolves
//! `<working_dir>/.hamlet`, delegates to [`Ledger`], and renders deterministic
//! output derived strictly from `charter.json` and the `.hamlet/` tree.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use hamlet_core::tree::HamletTree;
use hamlet_daemon::ledger::{
    render_commission_list, CommissionCloseReport, CommissionDetail, CommissionSummary,
    CreatedCommission, Ledger, LedgerError,
};

/// Errors returned by `hamlet commission` subcommands.
#[derive(Debug)]
pub enum CommissionCliError {
    /// Bad CLI invocation (missing required flag, unknown subcommand, extra
    /// positional arguments) — maps to exit code `2`.
    Usage(String),
    /// Runtime / validation / ledger failure — maps to exit code `1`.
    Ledger(LedgerError),
    /// Brief file / stdin read failure — maps to exit code `1`.
    BriefIo {
        /// Source description (`stdin` or file path).
        source_desc: String,
        /// Underlying OS error.
        source: io::Error,
    },
}

impl fmt::Display for CommissionCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg) => write!(f, "{msg}"),
            Self::Ledger(e) => write!(f, "{e}"),
            Self::BriefIo {
                source_desc,
                source,
            } => write!(f, "read brief from {source_desc}: {source}"),
        }
    }
}

impl std::error::Error for CommissionCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ledger(e) => Some(e),
            Self::BriefIo { source, .. } => Some(source),
            Self::Usage(_) => None,
        }
    }
}

impl From<LedgerError> for CommissionCliError {
    fn from(e: LedgerError) -> Self {
        Self::Ledger(e)
    }
}

/// Source for the `COMMISSION.md` brief passed to `--brief`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefSource {
    /// Read brief from `stdin` (`--brief -`).
    Stdin,
    /// Read brief from a filesystem path (`--brief <file>`).
    File(PathBuf),
}

/// Parsed arguments for `hamlet commission new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewArgs {
    /// Raw commission slug (sanitized by [`Ledger::create_commission`]).
    pub slug: String,
    /// Source of the `COMMISSION.md` brief.
    pub brief: BriefSource,
    /// Optional `--seed <dir>` directory to import as initial commit on `main`.
    pub seed: Option<PathBuf>,
    /// Optional `--dir <working_dir>` override (defaults to `cwd`).
    pub working_dir: Option<PathBuf>,
}

/// Parsed arguments for `hamlet commission list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListArgs {
    /// Optional `--dir <working_dir>` override (defaults to `cwd`).
    pub working_dir: Option<PathBuf>,
}

/// Parsed arguments for `hamlet commission show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowArgs {
    /// Raw commission slug to inspect.
    pub slug: String,
    /// Optional `--dir <working_dir>` override (defaults to `cwd`).
    pub working_dir: Option<PathBuf>,
}

/// Parsed arguments for `hamlet commission close`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseArgs {
    /// Raw commission slug to close.
    pub slug: String,
    /// Optional `--dir <working_dir>` override (defaults to `cwd`).
    pub working_dir: Option<PathBuf>,
}

/// Parsed `hamlet commission` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommissionCommand {
    /// `hamlet commission new <slug> --brief <file|-> [--seed <dir>]`
    New(NewArgs),
    /// `hamlet commission list`
    List(ListArgs),
    /// `hamlet commission show <slug>`
    Show(ShowArgs),
    /// `hamlet commission close <slug>`
    Close(CloseArgs),
}

/// Parse argv slice following `hamlet commission`.
///
/// # Errors
///
/// Returns [`CommissionCliError::Usage`] when the subcommand or flags are
/// invalid.
pub fn parse_args(args: &[&str]) -> Result<CommissionCommand, CommissionCliError> {
    let Some((&sub, rest)) = args.split_first() else {
        return Err(CommissionCliError::Usage(
            "missing commission subcommand (expected: new | list | show | close)".to_owned(),
        ));
    };
    match sub {
        "new" => parse_new_args(rest).map(CommissionCommand::New),
        "list" => parse_list_args(rest).map(CommissionCommand::List),
        "show" => parse_show_args(rest).map(CommissionCommand::Show),
        "close" => parse_close_args(rest).map(CommissionCommand::Close),
        other => Err(CommissionCliError::Usage(format!(
            "unknown commission subcommand '{other}' (expected: new | list | show | close)"
        ))),
    }
}

fn parse_new_args(args: &[&str]) -> Result<NewArgs, CommissionCliError> {
    let mut slug: Option<String> = None;
    let mut brief: Option<BriefSource> = None;
    let mut seed: Option<PathBuf> = None;
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
                return Err(CommissionCliError::Usage(
                    "usage: hamlet commission new <slug> --brief <file|-> [--seed <dir>]"
                        .to_owned(),
                ));
            }
            if arg == "--brief" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(CommissionCliError::Usage(
                        "--brief requires an argument (<file|->)".to_owned(),
                    ));
                };
                if brief.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--brief specified more than once".to_owned(),
                    ));
                }
                brief = Some(parse_brief_source(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--brief=") {
                if val.is_empty() {
                    return Err(CommissionCliError::Usage(
                        "--brief requires an argument (<file|->)".to_owned(),
                    ));
                }
                if brief.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--brief specified more than once".to_owned(),
                    ));
                }
                brief = Some(parse_brief_source(val));
                i += 1;
                continue;
            }
            if arg == "--seed" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(CommissionCliError::Usage(
                        "--seed requires a directory path".to_owned(),
                    ));
                };
                if seed.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--seed specified more than once".to_owned(),
                    ));
                }
                seed = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--seed=") {
                if val.is_empty() {
                    return Err(CommissionCliError::Usage(
                        "--seed requires a directory path".to_owned(),
                    ));
                }
                if seed.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--seed specified more than once".to_owned(),
                    ));
                }
                seed = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if arg == "--dir" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(CommissionCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                };
                if working_dir.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--dir=") {
                if val.is_empty() {
                    return Err(CommissionCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                }
                if working_dir.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
        }

        if slug.is_none() {
            slug = Some(arg.to_owned());
            i += 1;
        } else {
            return Err(CommissionCliError::Usage(format!(
                "unexpected argument '{arg}' (usage: hamlet commission new <slug> --brief <file|-> [--seed <dir>])"
            )));
        }
    }

    let Some(slug) = slug else {
        return Err(CommissionCliError::Usage(
            "missing <slug> (usage: hamlet commission new <slug> --brief <file|-> [--seed <dir>])"
                .to_owned(),
        ));
    };
    let Some(brief) = brief else {
        return Err(CommissionCliError::Usage(
            "missing required --brief <file|-> flag".to_owned(),
        ));
    };

    Ok(NewArgs {
        slug,
        brief,
        seed,
        working_dir,
    })
}

fn parse_list_args(args: &[&str]) -> Result<ListArgs, CommissionCliError> {
    let mut working_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--dir" {
            i += 1;
            let Some(&val) = args.get(i) else {
                return Err(CommissionCliError::Usage(
                    "--dir requires a directory path".to_owned(),
                ));
            };
            if working_dir.is_some() {
                return Err(CommissionCliError::Usage(
                    "--dir specified more than once".to_owned(),
                ));
            }
            working_dir = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--dir=") {
            if val.is_empty() {
                return Err(CommissionCliError::Usage(
                    "--dir requires a directory path".to_owned(),
                ));
            }
            if working_dir.is_some() {
                return Err(CommissionCliError::Usage(
                    "--dir specified more than once".to_owned(),
                ));
            }
            working_dir = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        return Err(CommissionCliError::Usage(format!(
            "unexpected argument '{arg}' (usage: hamlet commission list)"
        )));
    }
    Ok(ListArgs { working_dir })
}

fn parse_show_args(args: &[&str]) -> Result<ShowArgs, CommissionCliError> {
    let mut slug: Option<String> = None;
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
                return Err(CommissionCliError::Usage(
                    "usage: hamlet commission show <slug>".to_owned(),
                ));
            }
            if arg == "--dir" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(CommissionCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                };
                if working_dir.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--dir=") {
                if val.is_empty() {
                    return Err(CommissionCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                }
                if working_dir.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
        }
        if slug.is_none() {
            slug = Some(arg.to_owned());
            i += 1;
        } else {
            return Err(CommissionCliError::Usage(format!(
                "unexpected argument '{arg}' (usage: hamlet commission show <slug>)"
            )));
        }
    }
    let Some(slug) = slug else {
        return Err(CommissionCliError::Usage(
            "missing <slug> (usage: hamlet commission show <slug>)".to_owned(),
        ));
    };
    Ok(ShowArgs { slug, working_dir })
}

fn parse_close_args(args: &[&str]) -> Result<CloseArgs, CommissionCliError> {
    let mut slug: Option<String> = None;
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
                return Err(CommissionCliError::Usage(
                    "usage: hamlet commission close <slug>".to_owned(),
                ));
            }
            if arg == "--dir" {
                i += 1;
                let Some(&val) = args.get(i) else {
                    return Err(CommissionCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                };
                if working_dir.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--dir=") {
                if val.is_empty() {
                    return Err(CommissionCliError::Usage(
                        "--dir requires a directory path".to_owned(),
                    ));
                }
                if working_dir.is_some() {
                    return Err(CommissionCliError::Usage(
                        "--dir specified more than once".to_owned(),
                    ));
                }
                working_dir = Some(PathBuf::from(val));
                i += 1;
                continue;
            }
        }
        if slug.is_none() {
            slug = Some(arg.to_owned());
            i += 1;
        } else {
            return Err(CommissionCliError::Usage(format!(
                "unexpected argument '{arg}' (usage: hamlet commission close <slug>)"
            )));
        }
    }
    let Some(slug) = slug else {
        return Err(CommissionCliError::Usage(
            "missing <slug> (usage: hamlet commission close <slug>)".to_owned(),
        ));
    };
    Ok(CloseArgs { slug, working_dir })
}

fn parse_brief_source(raw: &str) -> BriefSource {
    if raw == "-" {
        BriefSource::Stdin
    } else {
        BriefSource::File(PathBuf::from(raw))
    }
}

/// Read the brief body from a file or `stdin`, refusing symlink brief paths
/// under ADR-0007.
///
/// # Errors
///
/// Returns [`CommissionCliError`] on symlink refusal or I/O failure.
pub fn load_brief(source: &BriefSource) -> Result<String, CommissionCliError> {
    match source {
        BriefSource::Stdin => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .map_err(|source| CommissionCliError::BriefIo {
                    source_desc: "stdin".to_owned(),
                    source,
                })?;
            Ok(buf)
        }
        BriefSource::File(path) => {
            let meta =
                fs::symlink_metadata(path).map_err(|source| CommissionCliError::BriefIo {
                    source_desc: path.display().to_string(),
                    source,
                })?;
            if meta.file_type().is_symlink() {
                return Err(CommissionCliError::Ledger(LedgerError::SymlinkRefused {
                    path: path.clone(),
                }));
            }
            if !meta.is_file() {
                return Err(CommissionCliError::BriefIo {
                    source_desc: path.display().to_string(),
                    source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
                });
            }
            fs::read_to_string(path).map_err(|source| CommissionCliError::BriefIo {
                source_desc: path.display().to_string(),
                source,
            })
        }
    }
}

/// Execute `hamlet commission new` against `working_dir` (which contains
/// `.hamlet/`).
///
/// # Errors
///
/// Returns [`CommissionCliError`] on invalid brief/slug/seed or ledger error.
pub fn run_new(
    working_dir: &Path,
    slug: &str,
    brief_text: &str,
    seed: Option<&Path>,
) -> Result<CreatedCommission, CommissionCliError> {
    let tree = HamletTree::new(working_dir.join(".hamlet"));
    let ledger = Ledger::new(tree);
    Ok(ledger.create_commission(slug, brief_text, seed)?)
}

/// Execute `hamlet commission list` against `working_dir` (which contains
/// `.hamlet/`).
///
/// # Errors
///
/// Returns [`CommissionCliError`] if the village is not initialized or
/// `charter.json` cannot be loaded.
pub fn run_list(working_dir: &Path) -> Result<Vec<CommissionSummary>, CommissionCliError> {
    let tree = HamletTree::new(working_dir.join(".hamlet"));
    let ledger = Ledger::new(tree);
    Ok(ledger.list_commissions()?)
}

/// Execute `hamlet commission show <slug>` against `working_dir` (which
/// contains `.hamlet/`).
///
/// # Errors
///
/// Returns [`CommissionCliError`] if `<slug>` is invalid, unknown, or the
/// village is not initialized.
pub fn run_show(working_dir: &Path, slug: &str) -> Result<CommissionDetail, CommissionCliError> {
    let tree = HamletTree::new(working_dir.join(".hamlet"));
    let ledger = Ledger::new(tree);
    Ok(ledger.show_commission(slug)?)
}

/// Execute `hamlet commission close <slug>` against `working_dir` (which
/// contains `.hamlet/`).
///
/// # Errors
///
/// Returns [`CommissionCliError`] if `<slug>` is invalid, unknown, or the
/// village is not initialized.
pub fn run_close(
    working_dir: &Path,
    slug: &str,
) -> Result<CommissionCloseReport, CommissionCliError> {
    let tree = HamletTree::new(working_dir.join(".hamlet"));
    let ledger = Ledger::new(tree);
    Ok(ledger.close_commission(slug)?)
}

/// Execute a parsed [`CommissionCommand`] using `default_working_dir` when
/// `--dir` was not provided, returning the rendered stdout string.
///
/// # Errors
///
/// Returns [`CommissionCliError`] on usage, I/O, or ledger failure.
pub fn execute(
    cmd: &CommissionCommand,
    default_working_dir: &Path,
) -> Result<String, CommissionCliError> {
    match cmd {
        CommissionCommand::New(args) => {
            let dir = args.working_dir.as_deref().unwrap_or(default_working_dir);
            let brief_text = load_brief(&args.brief)?;
            let created = run_new(dir, &args.slug, &brief_text, args.seed.as_deref())?;
            Ok(created.summary())
        }
        CommissionCommand::List(args) => {
            let dir = args.working_dir.as_deref().unwrap_or(default_working_dir);
            let items = run_list(dir)?;
            Ok(render_commission_list(&items))
        }
        CommissionCommand::Show(args) => {
            let dir = args.working_dir.as_deref().unwrap_or(default_working_dir);
            let detail = run_show(dir, &args.slug)?;
            Ok(detail.render())
        }
        CommissionCommand::Close(args) => {
            let dir = args.working_dir.as_deref().unwrap_or(default_working_dir);
            let report = run_close(dir, &args.slug)?;
            Ok(report.summary())
        }
    }
}
