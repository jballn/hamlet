//! `hamlet` CLI binary (SPEC §12.1; flags [DEV] → ADR sprint 08).
//!
//! Normative command surface: `init`, `daemon {start|stop|status}`,
//! `commission {new|list|show|close}`, `agent {list|hibernate|wake|steer}`,
//! `decree {list|show}`, `vault {list|export}`, `metrics {show|export}`,
//! `mail {send|read}`. The CLI holds no orchestration state; it drives the
//! daemon and reads tree truth. Stub only: prints version and exits.
//!
//! // TODO(sprint 08): clap argument parser + subcommands. clap is on the
//! // dev-dependency allowlist only — moving it to runtime requires an ADR
//! // and a `scripts/check-deps.sh` allowlist update (CONVENTIONS §2).

fn main() {
    println!(
        "hamlet {} (stub — command surface lands in sprint 08)",
        env!("CARGO_PKG_VERSION")
    );
}
