//! `hamlet` CLI binary (SPEC §12.1; flags [DEV] → ADR sprint 08).
//!
//! Normative command surface: `init`, `daemon {start|stop|status}`,
//! `commission {new|list|show|close}`, `agent {list|hibernate|wake|steer}`,
//! `decree {list|show}`, `vault {list|export}`, `metrics {show|export}`,
//! `mail {send|read}`. The CLI holds no orchestration state; it drives the
//! daemon and reads tree truth. Sprint 01 wires `init`; the remaining
//! subcommands arrive with their sprints.
//!
//! // TODO(sprint 08): clap argument parser + subcommands. clap is on the
//! // dev-dependency allowlist only — moving it to runtime requires an ADR
//! // and a `scripts/check-deps.sh` allowlist update (CONVENTIONS §2).

use std::path::Path;
use std::process::ExitCode;

use hamlet_cli::commission::{self, CommissionCliError};
use hamlet_cli::init;
use hamlet_cli::vault::{self, VaultCliError};

/// Exit code for usage errors (distinct from runtime failure).
fn exit_usage() -> ExitCode {
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let owned: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] | ["help"] | ["--help"] | ["-h"] => {
            print_usage();
            ExitCode::SUCCESS
        }
        ["--version"] | ["-V"] => {
            println!("hamlet {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["init", dir] => match init::run(Path::new(dir)) {
            Ok(report) => {
                println!("hamlet init: {}", report.village_root.display());
                for line in report.lines() {
                    println!("  {line}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("hamlet init: error: {e}");
                ExitCode::FAILURE
            }
        },
        ["init"] => {
            eprintln!("hamlet init: error: missing <dir> argument");
            print_usage();
            exit_usage()
        }
        ["commission", rest @ ..] => {
            let cmd = match commission::parse_args(rest) {
                Ok(cmd) => cmd,
                Err(CommissionCliError::Usage(msg)) => {
                    eprintln!("hamlet commission: error: {msg}");
                    print_usage();
                    return exit_usage();
                }
                Err(e) => {
                    eprintln!("hamlet commission: error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let cwd = match std::env::current_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    eprintln!("hamlet commission: error: current_dir: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match commission::execute(&cmd, &cwd) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(CommissionCliError::Usage(msg)) => {
                    eprintln!("hamlet commission: error: {msg}");
                    print_usage();
                    exit_usage()
                }
                Err(e) => {
                    eprintln!("hamlet commission: error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        ["vault", rest @ ..] => {
            let cmd = match vault::parse_args(rest) {
                Ok(cmd) => cmd,
                Err(VaultCliError::Usage(msg)) => {
                    eprintln!("hamlet vault: error: {msg}");
                    print_usage();
                    return exit_usage();
                }
                Err(e) => {
                    eprintln!("hamlet vault: error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let cwd = match std::env::current_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    eprintln!("hamlet vault: error: current_dir: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match vault::execute(&cmd, &cwd) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(VaultCliError::Usage(msg)) => {
                    eprintln!("hamlet vault: error: {msg}");
                    print_usage();
                    exit_usage()
                }
                Err(e) => {
                    eprintln!("hamlet vault: error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        [cmd, ..] => {
            eprintln!("hamlet: error: unknown or incomplete command: {cmd}");
            print_usage();
            exit_usage()
        }
    }
}

fn print_usage() {
    print!(
        "\
hamlet — containerized multi-agent orchestration engine

USAGE:
    hamlet <COMMAND>

COMMANDS:
    init <dir>                                          Create the normative .hamlet/ village tree inside <dir>
                                                        (idempotent; never clobbers charter.json/config.json/.env)
    commission new <slug> --brief <file|-> [--seed <d>] Register a new commission, bare git repo, and COMMISSION.md
    commission list                                     List all commissions from charter.json + tree
    commission show <slug>                              Show commission details from charter.json + tree
    commission close <slug>                             Close a commission, spin down sessions, and archive telemetry
    vault list [--commission <slug>]                    List promoted deliverables in keep/vault/
    vault export <artifact> <dest> [--commission <s>]   Export a promoted deliverable to <dest>
    help                                                Print this message

Remaining commands (daemon, agent, decree, metrics, mail)
land with their sprints; see SPEC §12.1.
"
    );
}
