//! Agent session supervision (SPEC §6; supervision details [DEV] → ADR).
//!
//! Owns pi session process lifecycle over `podman exec` stdio: spawn,
//! health, restart policy, graceful stop, hard kill on hibernation. Injects
//! out-of-band `interrupt_and_steer` packets (Divine Interruption). Crash
//! telemetry → sled; crash lineage → TITLE.md.
//!
//! // TODO(sprint 03)
