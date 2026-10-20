//! `hamlet-daemon` binary entrypoint (SPEC §3).

use hamlet_core::tree::HamletTree;
use hamlet_daemon::local_api::serve_daemon;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from(".");
    let mut port = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" if i + 1 < args.len() => {
                dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                port = args[i + 1].parse::<u16>().ok();
                i += 2;
            }
            "--version" | "-V" => {
                println!("hamlet-daemon {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => {
                i += 1;
            }
        }
    }

    let hamlet_root = if dir.ends_with(".hamlet") {
        dir
    } else {
        dir.join(".hamlet")
    };

    let tree = Arc::new(HamletTree::new(hamlet_root));
    if let Err(e) = serve_daemon(tree, port, None).await {
        eprintln!("hamlet-daemon: error: {e}");
        std::process::exit(1);
    }
    Ok(())
}
