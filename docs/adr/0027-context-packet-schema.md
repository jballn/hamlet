# ADR-0027 — Context packet: MAYOR.md + state grid + one event artifact

Status: Accepted
Sprint: 05-mayor-loop · Prompt: 05/01-architect
Refs: SPEC §7 (stateless executive, context packet), §11.1; CONVENTIONS §3 r1
/ r3 / r4; ADR-0004 (canonical JSON), ADR-0026 cl. 3 (`next_decree_seq`)

## Context

SPEC §7: the Mayor keeps **no chat thread** — on every wake the daemon must
reconstitute the whole prompt, and nothing may accumulate between turns. Two
encodings were left open: how the charter reaches the Mayor (raw
`charter.json` text vs a typed projection) and how the wake cause is carried.
CONVENTIONS §3 r3 wants canonical sorted JSON; §3 r4 forbids clock-derived
state, so the packet's decree cursor must come from the charter counter.

## Decision

1. **`ContextPacket` = the entire prompt**, four parts in a fixed section
   order: `MAYOR.md` verbatim, the state grid, the event block, the decree
   protocol reminder. There is **no history field, no buffer, no "previous
   turns"** — one packet, one Mayor turn, then sleep at zero token cost.
2. **`StateGrid`** is a typed charter *subset* (never the raw charter text):
   `charter_version`, `next_decree_seq` (the ADR-0026 cl. 3 cursor — how the
   stateless Mayor knows which sequence to stamp), open (queued) decrees,
   commissions, parcels, sessions, slots. Rendered as canonical pretty JSON
   with sorted keys at every level (`BTreeMap` groups), trailing newline, so
   the projection is human-readable (SPEC §7) and byte-stable goldens.
3. **`EventArtifact`** is exactly one typed event per packet, each with a
   verbatim body bounded to 64 KiB: `MailReceived` (`MailSender`, subject,
   spool `TreeRelPath`), `WebhookEvent` (topic, `CorrelationId`, status),
   `FreightViolation` (sandbox, owner, `ViolationKind`s — ADR-0021 cl. 5
   evidence), `DecreeAckTimeout` (decree id, `waited_ms`),
   `CommissionCreated` (index), `GitDiffReady` (commission, base, head).
4. **`WakeReason`** (`mail_to_mayor`, `webhook_event`, `freight_violation`,
   `decree_ack_timeout`, `commission_created`, `git_diff_ready`) is **derived**
   from the artifact by the constructor, so reason and event can never
   disagree; the daemon cannot label a wake inconsistently.
5. **Caps are refusals, never truncations** (a truncated prompt is a corrupted
   one): `MAYOR.md` ≤256 KiB, artifact body ≤64 KiB, packet ≤512 KiB
   (`PacketTooLarge`). Rendering is a pure function of the typed fields, so
   equal state always yields identical bytes (no timestamps in the prompt).
6. The packet's trailing **decree protocol reminder** (dir, file naming,
   digest rule, `next_decree_seq`, ack semantics) is what makes a stateless
   executive self-sufficient: the Mayor never needs to remember the protocol.

## Consequences

- A wake is always caused by something derivable from the tree; the packet
  cannot smuggle in daemon-only knowledge, and every field is either MAYOR.md
  text, a charter projection, or a tree artifact.
- `waited_ms` is telemetry-shaped and never a state input (CONVENTIONS §3 r4);
  ordering continues to come from `next_decree_seq` alone.
- Sprint 05 step 02 consumes these types to build packets from the Sprint 04
  reaction artifacts (`MayorWake`, `WebhookWake`, mail router, acks); a new
  wake source is one `EventKind` variant, not a schema change.
