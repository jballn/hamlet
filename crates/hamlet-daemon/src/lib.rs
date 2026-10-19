//! # hamlet-daemon — the singular governor (ADR-0001)
//!
//! The only crate that spawns processes (podman, pi sessions) or mutates a
//! live `.hamlet/` tree at runtime (CONVENTIONS §2; SPEC §3 layering rule).
//! All pure logic — tree layout, sanitizer, charter model, schemas — lives in
//! `hamlet-core` and is consumed here; the daemon implements core's
//! injected side-effect traits (fs, process, clock-free telemetry sinks).
//!
//! Module map (module → SPEC → sprint):
//!
//! | Module | Responsibility | SPEC | Sprint |
//! |---|---|---|---|
//! | [`podman`] | rootless podman CLI layer over tokio pipes | §4 | 02 |
//! | [`bootstrap`] | blueprint execution: derived image, toolchains, profile | §4, §5 | 02 |
//! | [`hostperm`] | host permission model: modes + subuid ownership | §4 | 02 |
//! | [`supervise`] | agent session lifecycle: spawn/health/restart/stop | §6 | 03 |
//! | [`delivery`] | persistent per-session delivery queues (stdin funnel) | §6, §8.1 | 03 |
//! | [`metrics`] | stream telemetry extractor: JSON-RPC → sled observations | §6, §11.2 | 03 |
//! | [`watch`] | notify-driven tree watchers (mail, DEED, decrees) | §6, §8 | 04 |
//! | [`mailroute`] | spool → sequential recipient stdin delivery | §8.1 | 04 |
//! | [`freight`] | freight ACL sandboxes, vault promotion copies | §4, §7 | 04 |
//! | [`dispatch`] | webhook outbox/inbox over reqwest, inbound axum hooks | §8.4–5 | 04 |
//! | [`web`] | axum server + WS (frontend assets, metrics stream) | §12.3 | 04/09 |
//! | [`mayor`] | Mayor wake loop, context packets, decree interpretation | §7 | 05 |
//! | [`ledger`] | commission lifecycle, git mediation, convergence | §9 | 06 |
//! | [`census`] | parcel provisioning (02); hibernation, awakening | §10 | 02/07 |
//! | [`decrees`] | decree interpretation: validate → replay-check → execute → ack | §7 | 05 |
//! | [`telemetry`] | sled metrics sink (never orchestration state) | §11.2 | 01 |
//! | [`promote`] | DEED alteration validation → blueprint evolution | §5, §9.4 | 06 |

pub mod bootstrap;
pub mod census;
pub mod decrees;
pub mod delivery;
pub mod dispatch;
pub mod freight;
pub mod hostperm;
pub mod ledger;
pub mod mailroute;
pub mod mayor;
pub mod metrics;
pub mod podman;
pub mod promote;
pub mod supervise;
pub mod telemetry;
pub mod vault;
pub mod watch;
pub mod web;
