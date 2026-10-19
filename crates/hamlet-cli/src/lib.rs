//! `hamlet` CLI logic (SPEC §12.1).
//!
//! The CLI holds **no orchestration state** (CONVENTIONS §2): it bootstraps
//! trees via [`init`], then drives the daemon and reads tree truth. Command
//! surface flags are [DEV] → ADR in sprint 08; argument handling in
//! `main.rs` is intentionally hand-rolled until then (clap is dev-allowlist
//! only).

pub mod commission;
pub mod init;
pub mod vault;
