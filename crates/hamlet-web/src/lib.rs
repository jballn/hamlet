//! # hamlet-web — WASM 8-bit RPG frontend (SPEC §12.3, stage 3)
//!
//! Asset layers + Rust/JS glue served by the daemon's axum server;
//! interactable sprites map 1:1 to the village landmarks of SPEC §2. Work
//! (commissions, decrees, vault) flows through daemon endpoints only; the
//! frontend renders exclusively from daemon-pushed state over HTTP/WS — it
//! never mutates `.hamlet/` directly (ADR-0001).
//!
//! // TODO(sprint 09)
