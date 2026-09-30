//! Commission state machine (SPEC §9).
//!
//! Lifecycle types for `guild/ledger/commission<n>/`: creation
//! (COMMISSION.md + bare git repo), decomposition (INDENTURE.md),
//! execution (work branches), convergence (DEED write-back + vault
//! promotion), close. Pure state transitions mirrored into charter
//! per-commission state; git push mediation ([DEV] → ADR, sprint 06).
//!
//! // TODO(sprint 06)
