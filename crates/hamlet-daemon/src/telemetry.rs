//! sled telemetry sink (SPEC §11.2; CONVENTIONS §2 file-as-truth).
//!
//! Captures token tallies, API costs, latency spikes, and system event
//! traces into embedded sled — out-of-band, never in charter.json and never
//! orchestration state. Exportable summaries land in `keep/archives/` on
//! commission close. Timestamps are lineage/telemetry only, never state
//! inputs (CONVENTIONS §3 rule 4).
//!
//! // TODO(sprint 01)
