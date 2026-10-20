//! `hamlet` CLI binary (SPEC §12.1; ADR-0041, ADR-0042).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use hamlet_cli::client::ClientError;
use hamlet_cli::{agent, commission, completions, daemon, decree, init, mail, metrics, vault};

#[derive(Parser, Debug)]
#[command(
    name = "hamlet",
    version,
    about = "Containerized multi-agent orchestration engine (SPEC §12.1)"
)]
pub struct Cli {
    /// Target village root directory (defaults to current dir or finds .hamlet)
    #[arg(long, global = true)]
    pub dir: Option<PathBuf>,

    /// Strict JSON stdout output
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress human status output
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Verbose logging output
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create the normative .hamlet/ village tree inside <dir>
    Init {
        /// Target village directory
        dir: Option<PathBuf>,
        /// Optional blueprint template name
        #[arg(long)]
        blueprint: Option<String>,
    },
    /// Daemon lifecycle control
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Commission state machine
    Commission {
        #[command(subcommand)]
        action: CommissionAction,
    },
    /// Agent stasis, census, and steering
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Mayor decrees and human escalations
    Decree {
        #[command(subcommand)]
        action: DecreeAction,
    },
    /// Courier mail messaging
    Mail {
        #[command(subcommand)]
        action: MailAction,
    },
    /// Promoted deliverable vault
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// System and agent telemetry metrics
    Metrics {
        #[command(subcommand)]
        action: Option<MetricsAction>,
        /// Live telemetry stream
        #[arg(long)]
        live: bool,
    },
    /// Generate shell completions
    Completions {
        /// Target shell (bash, zsh, fish)
        shell: String,
    },
    /// Launch interactive MUD terminal UI (SPEC §12.2)
    Tui {
        /// Initial room to view (guild, tavern, courier, market, keep, manor)
        #[arg(long)]
        room: Option<String>,
        /// Scripted demo mode with mock fixtures and simulated event stream
        #[arg(long)]
        demo: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    /// Start daemon (daemonized in background by default; use --fg for foreground)
    Start {
        /// Run in foreground instead of daemonizing
        #[arg(long)]
        fg: bool,
        /// Listen port
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run daemon in foreground (alias for daemon start --fg)
    Run {
        /// Listen port
        #[arg(long)]
        port: Option<u16>,
    },
    /// Render operator heartbeat view
    Status,
    /// Stop running daemon over local API
    Stop,
}

#[derive(Subcommand, Debug)]
pub enum CommissionAction {
    /// List all commissions
    List,
    /// Register a new commission
    New {
        /// Commission slug
        #[arg(allow_hyphen_values = true)]
        slug: String,
        /// Brief file path or '-' for stdin
        #[arg(long)]
        brief: String,
        /// Optional seed directory
        #[arg(long)]
        seed: Option<PathBuf>,
    },
    /// Show commission details
    Show {
        /// Commission slug
        #[arg(allow_hyphen_values = true)]
        slug: String,
    },
    /// Close a commission
    Close {
        /// Commission slug
        #[arg(allow_hyphen_values = true)]
        slug: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentAction {
    /// List census of parcels and agents
    List {
        /// Filter by commission slug
        #[arg(long)]
        commission: Option<String>,
    },
    /// Show agent parcel details
    Show {
        /// Agent slot UID
        uid: u32,
    },
    /// Hibernate an agent parcel into patron stasis
    Hibernate {
        /// Agent slot UID
        uid: u32,
        /// Hibernation trigger reason
        #[arg(long)]
        trigger: Option<String>,
    },
    /// Awaken a hibernated patron into an active agent
    Wake {
        /// Hibernated patron name
        #[arg(allow_hyphen_values = true)]
        patron: String,
        /// Commission slug to bind to
        #[arg(long)]
        commission: Option<String>,
    },
    /// Inject a steering directive into an agent session
    Steer {
        /// Agent slot UID
        uid: u32,
        /// Steering directive prompt
        directive: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum DecreeAction {
    /// List all decrees
    List,
    /// Show details of a decree
    Show {
        /// Decree ID
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum MailAction {
    /// List mail for a recipient UID
    List {
        /// Recipient slot UID
        uid: u32,
    },
    /// Read a specific mail message or list recipient mail
    Read {
        /// Recipient slot UID
        uid: u32,
        /// Specific mail file name
        file: Option<String>,
    },
    /// Send courier mail to a recipient slot
    Send {
        /// Recipient slot UID
        to: u32,
        /// Subject line
        subject: String,
        /// Message body
        body: Option<String>,
        /// Sender actor name (default operator)
        #[arg(long)]
        from: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum VaultAction {
    /// List promoted deliverables in vault
    List {
        /// Filter by commission slug
        #[arg(long)]
        commission: Option<String>,
    },
    /// Export a promoted deliverable to destination
    Export {
        /// Artifact name
        #[arg(allow_hyphen_values = true)]
        artifact: String,
        /// Destination path
        dest: PathBuf,
        /// Commission slug
        #[arg(long)]
        commission: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MetricsAction {
    /// Show metrics summary
    Show,
    /// Export metrics to JSON file
    Export {
        /// Destination file path
        dest: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let is_help_or_version = e.kind() == clap::error::ErrorKind::DisplayHelp
                || e.kind() == clap::error::ErrorKind::DisplayVersion;
            if is_help_or_version {
                print!("{e}");
                return ExitCode::SUCCESS;
            }

            let is_json = std::env::args().any(|a| a == "--json");
            if is_json {
                let escaped = e.to_string().trim().replace('"', "\\\"").replace('\n', " ");
                println!("{{\"error\":\"{escaped}\",\"code\":2}}");
            } else {
                eprint!("{e}");
            }
            return ExitCode::from(2);
        }
    };

    let Some(command) = cli.command else {
        let _ = Cli::command().print_help();
        println!();
        return ExitCode::SUCCESS;
    };

    let dir = cli.dir.as_deref();
    let json = cli.json;

    let res = match command {
        Commands::Init {
            dir: target_dir,
            blueprint: _,
        } => {
            let village_dir = target_dir.as_deref().or(dir);
            match village_dir {
                Some(p) => execute_init(p, json),
                None => Err(ClientError::Usage("missing <dir> argument".to_string())),
            }
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start { fg, port } => daemon::start(dir, fg, port, json).await,
            DaemonAction::Run { port } => daemon::run(dir, port).await,
            DaemonAction::Status => daemon::status(dir, json).await,
            DaemonAction::Stop => daemon::stop(dir, json).await,
        },
        Commands::Commission { action } => match action {
            CommissionAction::List => commission::list(dir, json).await,
            CommissionAction::New { slug, brief, seed } => {
                commission::new(dir, &slug, &brief, seed.as_deref(), json).await
            }
            CommissionAction::Show { slug } => commission::show(dir, &slug, json).await,
            CommissionAction::Close { slug } => commission::close(dir, &slug, json).await,
        },
        Commands::Agent { action } => match action {
            AgentAction::List { commission } => agent::list(dir, commission.as_deref(), json).await,
            AgentAction::Show { uid } => agent::show(dir, uid, json).await,
            AgentAction::Hibernate { uid, trigger } => {
                agent::hibernate(dir, uid, trigger.as_deref(), json).await
            }
            AgentAction::Wake { patron, commission } => {
                agent::wake(dir, &patron, commission.as_deref(), json).await
            }
            AgentAction::Steer { uid, directive } => agent::steer(dir, uid, &directive, json).await,
        },
        Commands::Decree { action } => match action {
            DecreeAction::List => decree::list(dir, json).await,
            DecreeAction::Show { id } => decree::show(dir, &id, json).await,
        },
        Commands::Mail { action } => match action {
            MailAction::List { uid } => mail::list(dir, uid, json).await,
            MailAction::Read { uid, file } => mail::read(dir, uid, file.as_deref(), json).await,
            MailAction::Send {
                to,
                subject,
                body,
                from,
            } => mail::send(dir, from.as_deref(), to, &subject, body.as_deref(), json).await,
        },
        Commands::Vault { action } => match action {
            VaultAction::List { commission } => vault::list(dir, commission.as_deref(), json).await,
            VaultAction::Export {
                artifact,
                dest,
                commission,
            } => vault::export(dir, &artifact, &dest, commission.as_deref(), json).await,
        },
        Commands::Metrics { action, live } => match action {
            Some(MetricsAction::Show) | None => metrics::show(dir, live, json).await,
            Some(MetricsAction::Export { dest }) => {
                metrics::export(dir, dest.as_deref(), json).await
            }
        },
        Commands::Completions { shell } => completions::generate(&shell),
        Commands::Tui { room, demo } => {
            let tui_opts = hamlet_tui::TuiOptions {
                dir: cli.dir.clone(),
                endpoint: None,
                token: None,
                initial_room: room,
                demo,
            };
            hamlet_tui::run(tui_opts)
                .await
                .map(|()| String::new())
                .map_err(ClientError::Operational)
        }
    };

    match res {
        Ok(out) => {
            if !out.is_empty() {
                print!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            let code = err.exit_code();
            if json {
                let escaped = err.to_string().replace('"', "\\\"").replace('\n', " ");
                println!("{{\"error\":\"{escaped}\",\"code\":{code}}}");
            } else {
                eprintln!("hamlet: error: {err}");
            }
            ExitCode::from(code)
        }
    }
}

fn execute_init(dir: &Path, json: bool) -> Result<String, ClientError> {
    match init::run(dir) {
        Ok(report) => {
            if json {
                let json_val = serde_json::json!({
                    "status": "ok",
                    "village_root": report.village_root.display().to_string(),
                    "artifacts": report.lines(),
                });
                return serde_json::to_string_pretty(&json_val).map_err(|e| {
                    ClientError::Operational(format!("JSON serialization error: {e}"))
                });
            }
            let mut out = format!("hamlet init: {}\n", report.village_root.display());
            for line in report.lines() {
                out.push_str(&format!("  {line}\n"));
            }
            Ok(out)
        }
        Err(e) => Err(ClientError::Operational(format!("{e}"))),
    }
}
