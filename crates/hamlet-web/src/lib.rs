//! # hamlet-web — WASM 8-bit RPG frontend (SPEC §12.3; ADR-0044)
//!
//! Asset layers + Rust/JS glue served by the daemon's axum server;
//! interactable sprites map 1:1 to the village landmarks of SPEC §2. Work
//! (commissions, decrees, vault) flows through daemon endpoints only; the
//! frontend renders exclusively from daemon-pushed state over HTTP/WS — it
//! never mutates `.hamlet/` directly (ADR-0001, ADR-0044).
//!
//! # Module Architecture
//!
//! - [`tiles`]: Tile map definitions and landmark geometry mirroring SPEC §2.
//! - [`scene`]: Pure scene graph representation emitted for canvas presentation.
//! - [`view_model`]: Pure state machine transforming WS frames into scene graphs.
//! - [`ffi`]: C-ABI WebAssembly exports for the thin JS canvas blitter.

pub mod ffi;
pub mod panels;
pub mod scene;
pub mod tiles;
pub mod view_model;

pub use ffi::*;
pub use panels::*;
pub use scene::*;
pub use tiles::*;
pub use view_model::*;
