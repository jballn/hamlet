//! Hibernation/census types (SPEC §10; resource bounds [DEV] → ADR).
//!
//! Census parcel leases (`keep/census/parcel<uid>/` with TITLE.md chain of
//! title), hibernation records (`tavern/patrons/PROPRIETOR<name>/` with
//! `home.uid.bak/`), and quick-awakening matching (skill configuration →
//! patron profile → next available UID slot). Pure state derivation; the
//! UID mapping persists in charter.json even while compute is freed.
//!
//! // TODO(sprint 07)
