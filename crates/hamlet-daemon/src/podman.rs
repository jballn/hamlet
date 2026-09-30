//! Rootless podman invocation layer (SPEC §4).
//!
//! Shells out to the `podman` CLI through tokio process pipes — no libpod
//! bindings, no Docker, no privileged mode. Applies the normative mount
//! table from `hamlet_core::mount` verbatim and enforces the §4 isolation
//! invariants (guarded tests per CONVENTIONS §5).
//!
//! // TODO(sprint 02)
