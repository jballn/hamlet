//! `hamlet-daemon` binary entrypoint (SPEC §3).
//!
//! Will boot the tokio runtime, hydrate `.env` (dotenvy), load the charter,
//! and start the governor loops. Stub only: prints version and exits.
//!
//! // TODO(sprint 02): real startup (podman layer first).

fn main() {
    println!(
        "hamlet-daemon {} (stub — no governor loops yet)",
        env!("CARGO_PKG_VERSION")
    );
}
