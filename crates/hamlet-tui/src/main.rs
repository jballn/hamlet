//! `hamlet-tui` standalone binary (SPEC §12.2).

use clap::Parser;
use hamlet_tui::{run, TuiOptions};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "hamlet-tui",
    version,
    about = "MUD-style terminal interface over the hamlet daemon API (SPEC §12.2)"
)]
pub struct Cli {
    /// Target village directory (defaults to current dir or finds .hamlet)
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Explicit daemon endpoint (e.g. 127.0.0.1:8765)
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Explicit daemon auth token
    #[arg(long)]
    pub token: Option<String>,

    /// Initial room to view (guild, tavern, courier, market, keep, manor)
    #[arg(long)]
    pub room: Option<String>,

    /// Scripted demo mode with mock fixtures and simulated event stream
    #[arg(long)]
    pub demo: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let options = TuiOptions {
        dir: cli.dir,
        endpoint: cli.endpoint,
        token: cli.token,
        initial_room: cli.room,
        demo: cli.demo,
    };

    if let Err(err) = run(options).await {
        eprintln!("hamlet-tui error: {err}");
        std::process::exit(1);
    }
}
