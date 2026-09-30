//! Normative container mount table (SPEC §4).
//!
//! Typed encoding of the exact host→container mount set every agent
//! container receives: parcel home, `/var/mail/`, `/var/freight/`,
//! `/var/git/` (ro), `/var/pkgcache/`, `/var/context/` (ro), manor home
//! (Mayor only). The daemon consumes this table verbatim; deviations and
//! the §4 isolation invariants are QA-enforced against it.
//!
//! // TODO(sprint 02)
