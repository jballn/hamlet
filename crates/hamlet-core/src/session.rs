//! Agent session protocol types (SPEC §6; wire schema [DEV] → ADR).
//!
//! JSON-RPC stream types for pi coding-agent sessions over `podman exec`
//! stdio: sequential per-recipient mail delivery boundaries, and the
//! out-of-band Divine Interruption `interrupt_and_steer` packet (must be
//! distinguishable from queued mail). Also the telemetry event shapes the
//! daemon extracts from the stream (token counts, cost, latency, traces).
//!
//! // TODO(sprint 03)
