//! DEED/vault promotion parsing (SPEC §9.4, §10 dual-state convergence).
//!
//! Parses DEED.md `### Dynamic Alterations` blocks (environment layer →
//! blueprint write-back) and PROPRIETOR.md profiles (intelligence layer →
//! `tavern/patrons/`), plus vault promotion requests out of `/var/freight/`
//! into `keep/vault/`. Parsing/validation only — promotion copies are
//! daemon-performed, with every name through [`crate::sanitize`].
//!
//! // TODO(sprint 06)
