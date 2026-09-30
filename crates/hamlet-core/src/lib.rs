//! # hamlet-core — pure logic crate (ADR-0001)
//!
//! `SPEC.md` at the repo root is law; this crate implements its data model
//! and pure algorithms. Section map (module → SPEC):
//!
//! | Module | Responsibility | SPEC |
//! |---|---|---|
//! | [`tree`] | typed `.hamlet/` layout — the only source of village paths | §2 |
//! | [`sanitize`] | path/name sanitizer (allowlist regex, golden vectors) | §11.3, §4 |
//! | [`charter`] | state model + atomic canonical serde for `charter.json` | §11.1 |
//! | [`slots`] | UID slot allocation: 1000 Mayor, 1001–2000 subagents | §4 |
//! | [`config`] | `config.json` + `.env` schemas | §2, §11.4 |
//! | [`blueprint`] | blueprint TOML schema (`guild/blueprints/`) | §5 |
//! | [`pins`] | toolchain pin table: version+URL+sha256 per artifact | §4, §5 |
//! | [`mount`] | normative container mount table | §4 |
//! | [`session`] | agent session protocol types (JSON-RPC, interrupts) | §6 |
//! | [`mail`] | mail format + routing types | §8 |
//! | [`decree`] | decree vocabulary + encoding | §7 |
//! | [`commission`] | commission lifecycle state machine | §9 |
//! | [`promote`] | DEED/vault promotion parsing | §9.4, §10 |
//! | [`stasis`] | hibernation/census types | §10 |
//!
//! **Purity seam (CONVENTIONS §2, review-failable):** this crate performs no
//! filesystem, process, or network side effects, beyond the two sanctioned
//! exceptions — [`tree::HamletTree::ensure`] (ADR-0003) and
//! [`charter::Charter::load`]/[`charter::Charter::save_atomic`] (ADR-0004,
//! the atomic-write protocol is itself the unit under test). Side-effectful
//! operations are otherwise typed traits implemented and injected by
//! `hamlet-daemon`. Everything here must be unit-testable without podman or
//! agents, over `BTreeMap`/`BTreeSet` state with no wall-clock reads
//! (CONVENTIONS §3).

pub mod blueprint;
pub mod charter;
pub mod commission;
pub mod config;
pub mod decree;
pub mod mail;
pub mod mount;
pub mod pins;
pub mod promote;
pub mod sanitize;
pub mod session;
pub mod slots;
pub mod stasis;
pub mod tree;
