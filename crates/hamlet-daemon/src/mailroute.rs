//! Mail routing & delivery (SPEC §8.1; format from `hamlet_core::mail`).
//!
//! Watches `courier/mail/` spools and delivers messages sequentially to the
//! recipient agent's stdin, one task boundary at a time — never interleaved
//! mid-message (SPEC §6). Per-recipient FIFO; ordering beyond that is
//! [DEV] → ADR (sprint 04).
//!
//! // TODO(sprint 04)
