# ADR-0002 — Path/name sanitizer: §4 rules + SafeName newtype

Status: Accepted
Supersedes: —
Sprint: 01-host-topology · Prompt: 01-architect

## Context

SPEC §2/§11.3 require every externally-derived name (agent, Mayor, webhook,
mail recipient) to pass a sanitizer before touching disk; CONVENTIONS §4
locks the exact rules and reserves this ADR number. The `[DEV]` surface:
error taxonomy, check order, type-level enforcement, golden vectors.

## Decision

We will implement `hamlet-core::sanitize` exactly per CONVENTIONS §4:

- **Shape regex** (single source of truth):
  `^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)?$` — allowlist charset, no leading
  `-`, at most one internal `.` acting as a file-extension separator
  (no leading/trailing/multiple dots, so `.` and `..` can never match).
- **Length** 1–64 chars (`MAX_NAME_LEN`), checked before shape.
- **API**: `sanitize_name(&str) -> Result<SafeName, SanitizeError>`.
  `SafeName` is an opaque newtype whose inner string is private; the
  function is its only public constructor. Tree accessors taking external
  names accept `&SafeName` and nothing else — traversal is blocked by the
  type system, not convention.
- **Error taxonomy & check order** (first failure wins; part of the golden
  contract): `Empty → TooLong → PathSeparator → ControlChar → NonAscii →
  LeadingDash → DotName → InvalidShape`.
- **Serde**: `SafeName` serializes as a string; deserialization
  re-validates, so untrusted charter/webhook payloads cannot inject unsafe
  names.
- **Golden vectors**: committed in `sanitize.rs` tests — 19 accept + 35
  reject cases covering unicode, traversal, NUL/control chars, separators,
  overlength, and shape violations. Changes require a superseding ADR.

## Consequences

- Any name on disk under `.hamlet/` is a single safe path segment by
  construction; no accessor can be handed `../`, `/abs`, or control chars.
- Multi-dot names (`v1.2.3`, `a.tar.gz`) are rejected — deliverables needing
  multiple dots must encode them (e.g. `v1-2-3.tar.gz`); acceptable because
  promotions are daemon-performed and renames happen at the boundary.
- Every later consumer (mail routing, decrees, promotions, webhooks) must
  call `sanitize_name` at its trust boundary.
