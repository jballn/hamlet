//! `hamlet daemon` lifecycle control (SPEC §12.1; ADR-0041, ADR-0042).
//!
//! Subcommands:
//! - `daemon start [--fg] [--port <PORT>]`: Start the singular governor.
//! - `daemon run [--port <PORT>]`: Run foreground governor (alias for `start --fg`).
//! - `daemon status`: Render operator heartbeat view.
//! - `daemon stop`: Graceful shutdown over local API.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::api::*;
use hamlet_daemon::local_api::serve_daemon;

use crate::client::{resolve_village_tree, ClientError, HamletClient};

/// Execute `hamlet daemon start`.
pub async fn start(
    dir_flag: Option<&Path>,
    fg: bool,
    port: Option<u16>,
    json: bool,
) -> Result<String, ClientError> {
    let tree = resolve_village_tree(dir_flag)?;
    if !tree.charter().exists() {
        return Err(ClientError::Operational(format!(
            "village not initialized: charter.json missing in {}",
            tree.root().display()
        )));
    }

    if fg {
        return run_foreground(tree, port).await;
    }

    // Check if daemon is already running
    let pid_path = tree.daemon_pid();
    if pid_path.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if Path::new(&format!("/proc/{pid}")).exists() {
                    return Err(ClientError::Operational(format!(
                        "daemon is already running with PID {pid}"
                    )));
                }
            }
        }
    }

    // Spawn detached daemon process with readiness probe
    let exe = std::env::current_exe().map_err(|e| {
        ClientError::Operational(format!("cannot determine current executable: {e}"))
    })?;

    let mut cmd = Command::new(exe);
    cmd.arg("--dir").arg(tree.root());
    cmd.arg("daemon").arg("start").arg("--fg");
    if let Some(p) = port {
        cmd.arg("--port").arg(p.to_string());
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| ClientError::Operational(format!("failed to spawn daemon process: {e}")))?;

    // Readiness probe: poll local API endpoint until responsive or timeout
    let start_time = std::time::Instant::now();
    let timeout = Duration::from_secs(5);
    let mut last_err = String::from("timeout waiting for daemon readiness");

    while start_time.elapsed() < timeout {
        // Check if child exited prematurely
        if let Ok(Some(status)) = child.try_wait() {
            return Err(ClientError::Operational(format!(
                "daemon process exited prematurely with status {status}"
            )));
        }

        tokio::time::sleep(Duration::from_millis(50)).await;

        if let Ok(client) = HamletClient::discover(dir_flag) {
            if let Ok(status) = client.get::<DaemonStatusResponse>(ENDPOINT_STATUS).await {
                if json {
                    return serde_json::to_string_pretty(&status).map_err(|e| {
                        ClientError::Operational(format!("JSON serialization error: {e}"))
                    });
                }
                return Ok(format!(
                    "hamlet daemon started (PID: {}, endpoint: {})\n",
                    status.pid,
                    client.endpoint().base_url()
                ));
            }
        } else {
            last_err = "waiting for daemon.endpoint discovery file".to_string();
        }
    }

    // Timeout: try killing stalled child
    let _ = child.kill();
    Err(ClientError::Operational(format!(
        "daemon readiness probe timed out after 5s: {last_err}"
    )))
}

/// Execute `hamlet daemon run` (foreground daemon).
pub async fn run(dir_flag: Option<&Path>, port: Option<u16>) -> Result<String, ClientError> {
    let tree = resolve_village_tree(dir_flag)?;
    if !tree.charter().exists() {
        return Err(ClientError::Operational(format!(
            "village not initialized: charter.json missing in {}",
            tree.root().display()
        )));
    }
    run_foreground(tree, port).await
}

async fn run_foreground(
    tree: hamlet_core::tree::HamletTree,
    port: Option<u16>,
) -> Result<String, ClientError> {
    serve_daemon(Arc::new(tree), port, None)
        .await
        .map_err(|e| ClientError::Operational(format!("daemon runtime error: {e}")))?;
    Ok("Daemon stopped\n".to_string())
}

/// Execute `hamlet daemon status`.
pub async fn status(dir_flag: Option<&Path>, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let resp: DaemonStatusResponse = client.get(ENDPOINT_STATUS).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let out = format!(
        "Hamlet Daemon Status\n\
         --------------------\n\
         PID:             {}\n\
         Version:         {}\n\
         Uptime:          {}s\n\
         Village Root:    {}\n\
         Active Parcels:  {}\n\
         Commissions:     {}\n\
         Pending Decrees: {}\n\
         Queued Mail:     {}\n",
        resp.pid,
        resp.version,
        resp.uptime_secs,
        resp.village_root,
        resp.active_agents,
        resp.commissions_count,
        resp.queued_decrees,
        resp.queued_mail
    );

    Ok(out)
}

/// Execute `hamlet daemon stop`.
pub async fn stop(dir_flag: Option<&Path>, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let resp: DaemonStopResponse = client.post_empty(ENDPOINT_STOP).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(format!("hamlet daemon stopped (PID: {})\n", resp.pid))
}
