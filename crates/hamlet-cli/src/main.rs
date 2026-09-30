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

use hamlet_cli::init;

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
    init <dir>    Create the normative .hamlet/ village tree inside <dir>
                  (idempotent; never clobbers charter.json/config.json/.env)
    help          Print this message

Remaining commands (daemon, commission, agent, decree, vault, metrics, mail)
land with their sprints; see SPEC §12.1.
"
    );
}
