//! Typed `.hamlet/` directory layout (SPEC §2 — normative topology).
//!
//! Single source of truth for every village path: `guild/`, `tavern/`,
//! `courier/`, `market/`, `keep/`, `.env`, `charter.json`, `config.json`.
//! All consumers (daemon, cli) must build paths through this module — no
//! ad-hoc `format!`/`PathBuf::push` of village paths (CONVENTIONS §2).
//! Construction is pure; no I/O happens here.
//!
//! // TODO(sprint 01)
