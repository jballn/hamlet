//! Mail format + routing types (SPEC §8.1; header format [DEV] → ADR).
//!
//! Plain-text spool messages with minimal From/To/Subject headers, file
//! layout under `courier/mail/` per UID, and pure routing decisions
//! (recipient resolution, per-recipient FIFO ordering). Delivery side
//! effects belong to the daemon (`mailroute`), never here.
//!
//! // TODO(sprint 04)
