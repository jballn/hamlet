//! # hamlet-tui — MUD-style terminal frontend (SPEC §12.2, stage 2)
//!
//! Mocks the village layout as rooms = tree landmarks (guild/tavern/
//! courier/market/keep), agent sprites as text avatars, mail and decree
//! streams, commission board. Reads the same daemon API as every other
//! presentation layer — no new state, never mutates `.hamlet/` directly
//! (ADR-0001).
//!
//! // TODO(sprint 08)
