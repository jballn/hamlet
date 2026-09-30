//! Blueprint schema (SPEC §5; schema is [DEV] → requires an ADR).
//!
//! Immutable versioned TOML environment declarations in `guild/blueprints/`
//! (tools + versions: pixi packages, node versions, cargo toolchains,
//! apt/apk additions). Parsed here; executed by the daemon at parcel
//! provisioning. Also defines the parse target for blueprint evolution
//! write-back (SPEC §9.4, see [`crate::promote`]).
//!
//! // TODO(sprint 02)
