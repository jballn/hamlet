# ADR-0005 — sled telemetry: location, key layout & retention

Status: Accepted
Date: Sprint 01 (step 01/04)
Refs: SPEC §11.2; CONVENTIONS §2–§3, §9; ADR-0002, ADR-0003, ADR-0004

## Context

SPEC §11.2 puts high-velocity metrics (tokens, costs, latency spikes, traces)
in embedded sled — out-of-band, never in charter.json. Step 2 provisionally
defaulted `daemon_data_dir` to `.hamlet/sled`; this ADR relocates it.

## Decision

1. **Location — outside the tree.** `daemon_data_dir` (default
   `.hamlet-data/sled`) resolves against the *working directory containing*
   `.hamlet/`; `open_for_village` hard-rejects paths inside the village root.
   A binary, high-churn db in the tree would blur "tree + charter.json = all
   truth" and leak into watchers/git/archives. The store never reads the
   clock (§3 rule 4): `at_unix_ms` fields are caller-supplied.
2. **Namespaces.** Five schema-suffixed trees: `tokens.v1`, `costs.v1`
   (cumulative per session), `latency.v1`, `traces.v1` (append-only),
   `counters.v1` (per-agent seq allocators). Layout changes are new tree
   generations (`.v2`), never in-place reinterpretation.
3. **Keys.** Fixed-width big-endian, no delimiters: `commission u64 | uid
   u32 | session u64` (20 B); agent scope = 12-B prefix; append-only keys add
   `seq u64`; counters add `domain u8`. BE ordering makes sled's byte sort
   equal commission → uid → session → seq: queries/prunes are pure prefix
   scans. Values: canonical JSON records (`deny_unknown_fields`) except raw
   `u64` BE counters.
4. **Boundary is structural + tested.** Only allowlisted trees may exist;
   keys are fixed-width; a test fails on unknown trees, off-layout keys, or
   charter tokens (`slot`, `status`, `decree`, `parcel`, …) anywhere. Decode
   failures are `Corrupt` (fail-hard, ADR-0004).
5. **Retention — archives are the durable record.**
   `export_commission_summary` writes canonical JSON atomically (ADR-0004)
   to `keep/archives/commission-<n>-telemetry.json` via typed accessor
   (ADR-0003); wired at commission close in Sprint 06. After export,
   `prune_commission` may drop the commission's keys: sled is a rolling
   buffer, the archive is forever. Seq counters restart after pruning.
6. **Durability.** `flush()` is the checkpoint: flushed data survives
   `kill -9` (subprocess crash test); unflushed tails may be lost and are
   never assumed present. Telemetry loss degrades, never corrupts state.

## Consequences

- `config.json` golden + `resolve_daemon_data_dir` semantics changed; CLI
  fixtures updated in the same commit.
- Sprint 06 exports before pruning; nothing may read sled to answer "what is
  true" — only "what happened".
- New metrics need an additive `.v1` record field or a new tree generation.