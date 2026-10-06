# ADR-0026 — Decree protocol: vocabulary, encodings, acks, replay

Status: Accepted
Sprint: 05-mayor-loop · Prompt: 05/01-architect
Refs: SPEC §7, §4, §11.1/§11.3, §14 (`[DEV]` decree encodings); ADR-0027
(context packet, consumes `next_decree_seq`);
CONVENTIONS §2/§3/§4/§9; ADR-0002/0003/0004/0006; ADR-0019 cl. 5;
ADR-0021 cl. 5
Amends: ADR-0019 cl. 5 (`manor/home/**` row only)

## Context

SPEC §7 fixes the decree *vocabulary* but leaves encodings `[DEV]`. SPEC §4
mounts exactly one tree rw into the Mayor container
(`keep/census/manor/home/` → `/home/1000/`), settling the directory
question; CONVENTIONS §3 r4 forbids clock-derived ordering, so "which decree
first" cannot come from mtimes; and ADR-0019 cl. 5's provisional
`manor/home/**` reaction row must be narrowed now that the grammar exists.

## Decision

1. **Vocabulary** (internally tagged `decree_type`, per-variant key
   allowlist): `approve_promotion`, `reject_promotion`, `hibernate_agent`,
   `provision_agent`, `reassign_task`, `steer_session`, `escalate_to_human`.
   Typed at the boundary: `SafeName` refs, `u32` slot uid, `TreeRelPath`
   (freight paths, evidence labels), single-line `summary`/`reason` (≤280
   chars, no control chars), LF-only `directive` (≤8 KiB, CR rejected like
   ADR-0019 cl. 1), 1–8 `evidence_refs`; `freight_path` must sit under
   `courier/freight/` and address a file. SPEC §7's *decompose commission →
   INDENTURE.md* is **deferred to Sprint 06** (OBJECTIVE scope boundary) as a
   purely additive eighth variant.
2. **Directory/encoding:** `keep/census/manor/home/decrees/`
   (`/home/1000/decrees/`), created by `ensure` (no fresh-dir inotify race);
   one pretty-JSON file per decree, sorted keys at every level, trailing
   newline, ≤16 KiB, **temp + fsync + rename** by the Mayor. Names
   `<decree_id>.json`, `<decree_id>.ack`, `.rejected/`.
3. **Identity/ordering:** `<decree_id>` = `d-{seq:012}-{digest16}` — a valid
   `SafeName` whose lexicographic order equals sequence order. `seq` is the
   1-based charter counter (`next_decree_seq = max(decrees)+1`); `digest` is
   16 hex chars of FNV-1a/64 over the canonical body with `decree_id`
   removed — a *collision detector* for Mayor-authored files, not a security
   primitive (adversarial resistance = superseding ADR adding `sha2`).
4. **Admission/replay (idempotence):** charter **v3** adds
   `decree_receipts: seq → digest` (additive; `decrees` untouched).
   `admit_decree`: an unknown seq must equal `next_decree_seq` and is recorded
   queued *with* its digest in one atomic save; known seq + same digest =
   idempotent no-op (re-acked `done`); known seq + different digest = refused
   (`rejected: replay_mismatch`). Receipt and status key sets must mirror
   each other at load, so an id can never execute unadmitted.
5. **Acks:** the daemon writes `<decree_id>.ack` next to the decree —
   `{ack_version, decree_id, result}`, `result` = `done` | `rejected` + typed
   `RejectReason` (`replay_mismatch`, `unsupported_version`, `oversize`,
   `malformed`, `invalid_target`, `unknown_slot`, `execution_failed`). An
   unparseable file has no identity, so it is *renamed* into `.rejected/` and
   traced instead. No ack after `config.json`→`mayor.ack_timeout_ms` (default
   30 s, 1 000..=600 000) wakes the Mayor with a `decree_ack_timeout` event.
6. **Watcher (amends ADR-0019 cl. 5):** `Reaction::DecreeWrite` fires only
   for `manor/home/decrees/<decree_id>.json`; `.ack` writes, scratch files,
   and the rest of the Mayor home are `Ignored`, so a daemon acknowledgment
   never re-enters as a new decree.
7. **Strictness:** unknown JSON keys / unknown `decree_type` are `Schema`
   rejections; the digest binding is re-checked on parse, so a body edited
   under an unchanged id is `DigestMismatch`.

## Consequences

- The Mayor writes exactly one place in the tree; INDENTURE.md, vault
  promotion, and freight are daemon-performed from a decree, keeping podman
  and host commands out of its reach (SPEC §7).
- Charter 2→3, `config.json` 6→7 (additive `mayor` block); old documents still
  parse, but a v2 charter carrying `decrees` without receipts is *refused*,
  not repaired (no silent state invention). `decree_receipts` is the only
  replay record; sled stays telemetry-only (CONVENTIONS §2).
- Step 02 builds packets (ADR-0027), step 03 executes via `admit_decree` and
  writes acks; Sprint 06 adds `decompose_commission`.
