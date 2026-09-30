//! Census, hibernation & awakening execution (SPEC §10; bounds [DEV] → ADR 07).
//!
//! Provisions parcels, performs the hibernation routine (stop container →
//! copy `home/` to `tavern/patrons/PROPRIETOR<name>/home.uid.bak/` →
//! serialize PROPRIETOR.md → free compute; UID mapping persists in
//! charter), and quick-awakens patrons into the next available slot per
//! `hamlet_core::stasis` matching.
//!
//! // TODO(sprint 07)
