# 0019 — Mail format, spool lifecycle & watcher vocabulary

Status: Accepted (Sprint 04 prompts cite this as "ADR-0009"; that number was
claimed by the blueprint schema — see README renumbering table)

## Context

SPEC §8.1 leaves the mail header `[DEV]`. Sprint 04 needs a spool layout
with atomic writer visibility, ack semantics surviving daemon restart
without double-delivery, and a typed FsEvent → Reaction vocabulary shared by
the watcher (step 2), DEED mutations (Sprint 06), decrees (Sprint 05),
dispatch outbox, and freight dirs.

## Decision

1. **Format** (`hamlet-core::mail`): fixed-order headers `From:` (decimal
   UID or SafeName), `To:` (UID in slot range), `Subject:` (1–128 chars, no
   control chars), `Date:` (opaque lineage token ≤64 chars, never parsed
   into state — CONVENTIONS §3.4); blank line; UTF-8 body non-empty ≤64 KiB.
   LF-only — any CR is a parse rejection (injection defense). `parse`/
   `render` round-trip byte-exact; golden vectors committed.
2. **Spool**: `courier/mail/<recipient>/`; incoming = non-hidden `*.mail`.
   Writer grammar `{seq:012}-{writer_uid}-{nonce}.mail` (nonce ≤16 hex) so
   per-writer FIFO = lexicographic scan order; foreign names still accepted.
   Visibility: `.`-prefixed temp + rename; hidden components are ignored.
3. **Ack**: rename into `courier/mail/<recipient>/.read/` *after* the
   Sprint 03 `QueueStore` persisted the envelope (`courier/queues/<uid>.jsonl`)
   — enqueue-before-ack: crash between the two re-delivers once from spool,
   never drops. Restart scans spool top level only; `.read/` never rescanned.
   Ack collision: suffix `.r<n>` before the extension.
4. **Bounce**: undeliverable mail (no slot, unavailable session, overflow,
   malformed) → consumed into `.read/` + `From: system` bounce mail to the
   sender UID (typed `BounceReason`) + sled trace.
5. **Vocabulary** (`hamlet-core::watch`): pure `classify(&FsEvent) →
   Reaction` on village-relative paths — spool additions, `parcel<uid>/
   DEED.md` mutations, decree regions (`manor/home/**`,
   `commission<n>/INDENTURE.md`; Sprint 05's encoding ADR may extend the
   table — `classify` is the single mapping point), outbox additions,
   freight dir creations; else `Ignored`.
6. **Debounce**: per-file quiesce 250 ms (`QUIESCE_MS`); `EventBatch` dedups
   per path (Created dominates Modified; Created+Removed cancels;
   Removed+Created re-arms) and drains sorted by path; the daemon assigns
   monotonic sequence numbers at drain. Order = (path, seq), reproducible.

## Consequences

Core stays pure; the daemon owns inotify, renames, queueing. Mail state is
tree-derivable: spool = pending, `.read/` = consumed, queues = in-flight —
QA can prove zero double-delivery from the tree alone.
