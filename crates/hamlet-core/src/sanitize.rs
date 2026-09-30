//! Path/name sanitizer (SPEC §11.3; rules locked by CONVENTIONS §4, ADR-0002).
//!
//! Regex-based allowlist `[A-Za-z0-9._-]`, single internal `.`-suffix for
//! extensions, length 1–64; rejects `.`, `..`, leading `-`, control chars,
//! path separators, and non-allowlisted unicode. Every name derived from
//! agent, Mayor, or webhook input passes through here before touching disk.
//! Golden test vectors committed alongside the implementation.
//!
//! // TODO(sprint 01)
