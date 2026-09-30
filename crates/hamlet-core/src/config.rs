//! `config.json` + `.env` schemas (SPEC §2 tree roots, §11.4 secrets).
//!
//! Typed schema for orchestrator/engine settings (`config.json`) and the
//! `.env` secret surface (mode 0600, dotenvy-hydrated out-of-band). Secrets
//! never serialize into charter.json, archives, logs, or test fixtures
//! (CONVENTIONS §6); schema types must make leakage unrepresentable.
//!
//! // TODO(sprint 01)
