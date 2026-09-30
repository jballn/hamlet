//! Charter state model + atomic serde (SPEC §11.1; CONVENTIONS §3).
//!
//! In-memory state over `BTreeMap`/`BTreeSet` (sorted iteration, always);
//! canonical JSON with sorted keys; mandatory `charter_version: u32` with
//! additive-only migrations. Defines the serialize-to-temp → fsync → rename
//! atomic write protocol as a pure operation over an injected fs trait —
//! this module itself performs no I/O. Normative content groups: UID slot
//! table, agent registry refs, commission states, parcel leases, decree
//! queue status, webhook config refs.
//!
//! // TODO(sprint 01)
