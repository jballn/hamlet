//! UID slot allocation (SPEC §4; CONVENTIONS §3 rule 5).
//!
//! UID 1000 is reserved for the Mayor (manor). Subagent slots 1001–2000 are
//! allocated sequentially and are permanent per identity — never reused for
//! a different identity while a hibernated profile references them.
//! Exhaustion is a hard error surfaced to the operator, never a wrap.
//! Pure allocation logic over charter state; no I/O.
//!
//! // TODO(sprint 01)
