//! Commission lifecycle execution (SPEC §9; git mediation [DEV] → ADR).
//!
//! Creates `guild/ledger/commission<n>/` with COMMISSION.md and an
//! initialized bare git repo, drives decomposition/execution/convergence
//! transitions from `hamlet_core::commission`, mediates pushes to the
//! ro-mounted `/var/git/`, and applies DEED write-back + vault promotion at
//! commission end. Per-commission state mirrored into charter.json.
//!
//! // TODO(sprint 06)
