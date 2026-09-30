//! Freight ACLs & vault promotion execution (SPEC §4, §7 validation loop).
//!
//! Provides/permits POSIX ACL tooling (`setfacl`) inside containers but
//! never relaxes ACLs from the host on an agent's behalf. On Mayor approval,
//! sanitizes names (via `hamlet_core::sanitize`), copies freight artifacts
//! to `keep/vault/`, and spins down the session. Promotion copies are
//! daemon-performed only (SPEC §11.3).
//!
//! // TODO(sprint 04)
