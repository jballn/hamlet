//! The Mayor loop (SPEC §7; decree encodings [DEV] → ADR sprint 05).
//!
//! Wakes the stateless UID-1000 executive on events and builds pristine
//! single-turn context packets: MAYOR.md + charter state grid + targeted
//! event artifact. Interprets the Mayor's filesystem-emitted decrees
//! (vocabulary from `hamlet_core::decree`) and runs the deliverable
//! validation loop. The Mayor never executes podman or host commands —
//! it only writes files; this module interprets.
//!
//! // TODO(sprint 05)
