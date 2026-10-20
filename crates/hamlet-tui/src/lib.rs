//! # hamlet-tui — MUD-style terminal frontend (SPEC §12.2, stage 2)
//!
//! Mocks the village layout as rooms = tree landmarks (guild/tavern/
//! courier/market/keep/manor), agent sprites as text avatars, mail and decree
//! streams, commission board. Reads the same daemon API as every other
//! presentation layer — no new state, never mutates `.hamlet/` directly
//! (ADR-0001, ADR-0041, ADR-0042, ADR-0043).

pub mod app;
pub mod client;
pub mod demo;
pub mod model;
pub mod ui;

pub use app::{App, TuiOptions};
pub use client::TuiClient;
pub use model::{AppState, ConnectionStatus, ModalState, Room, VillageData};

/// Main runner entrypoint for `hamlet-tui`.
pub async fn run(options: TuiOptions) -> Result<(), String> {
    let mut app = App::new(options)?;
    app.run_loop().await
}
