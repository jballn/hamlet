//! `hamlet` CLI logic (SPEC §12.1; ADR-0041, ADR-0042).
//!
//! The CLI holds **no orchestration state** (CONVENTIONS §2): it bootstraps
//! trees via [`init`], then drives the daemon and reads tree truth over the
//! local HTTP API.

pub mod agent;
pub mod client;
pub mod commission;
pub mod completions;
pub mod daemon;
pub mod decree;
pub mod init;
pub mod mail;
pub mod metrics;
pub mod vault;
