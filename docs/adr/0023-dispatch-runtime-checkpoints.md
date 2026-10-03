# 0023 — Dispatch outbox runtime: attempt checkpoints & idempotent recovery

Status: Accepted (refines ADR-0020 cl. 4–5 for the daemon runtime; does not
change any ADR-0020 schema or policy value)

## Context

ADR-0020 fixes the outbox/inbox schemas, allowlist, deterministic retry
schedule and dead-letter rule, but leaves the *runtime* contract open:
where attempt state lives between retries, what a daemon restart mid-backoff
resumes from, and how double-sends are prevented. CONVENTIONS §2 forbids
in-daemon orchestration state (sled is telemetry only) — the answer must be
derivable from the tree.

## Decision

1. **The inbox file is the attempt checkpoint.** After every retryable
   failure (transport error, 429, 5xx) the daemon atomically writes
   `courier/dispatch/inbox/<correlation>.json` recording that attempt
   (`kind: "error"`, 1-based `attempt`). The terminal outcome overwrites
   the same file: success → `kind: "http"`; exhaustion → final
   `kind: "error"` at `attempt == max_attempts` + outbox rename into
   `dead/` (ADR-0020 cl. 5).
2. **Restart resumes from the checkpoint.** Processing an outbox file
   whose inbox checkpoint exists and is `kind: "error"` continues at
   `attempt + 1` with the same deterministic schedule — a restart never
   resets the budget to 1 and never skips ahead. A checkpoint at
   `attempt >= max_attempts` with the outbox file still present (crash
   between checkpoint and dead rename) completes the dead-letter rename
   without further network attempts.
3. **Delivery is idempotent on recovery.** `kind: "http"` checkpoint +
   outbox file present = crash between inbox write and outbox removal:
   recovery removes the outbox file, zero network attempts. The outbox
   file is removed (not archived) after a delivered response — the inbox
   file *is* the permanent record; `dead/` remains the only archive
   (never daemon-deleted, ADR-0020 cl. 5).
4. **Validation failures** (schema, oversize, allowlist, missing `.env`
   key) write an `attempt: 1` error inbox + immediate `dead/` rename,
   zero network attempts (ADR-0020 cl. 4); the secret map is re-read from
   `.env` at every send (cl. 6), never cached across requests.
5. **Concurrency**: one coordinator task consumes `DispatchOutbox`
   reactions (hints — ADR-0019 cl. 3) and spawns one worker per
   correlation, deduped by an in-flight set (process-local; the tree
   dedupes across restarts via cl. 2–3). A worker owns its file end-to-end,
   so backoff sleeps never block other correlations. Every attempt and
   terminal outcome gets a sled trace (`dispatch-attempt` /
   `dispatch-delivered` / `dispatch-dead` — correlation, attempt, method +
   URL authority only; never headers, body, or query).
6. **`dispatch.enabled=false`** freezes the plane: outbox files stay
   pending untouched (no inbox, no dead-letter) until re-enabled.

## Consequences

Kill-the-daemon-at-any-point restarts converge: each outbox file is
sent at most `max_attempts` times *in total* (not per process lifetime),
every state is a tree artifact, and the schedule stays a pure function
QA can assert tick-by-tick (ADR-0020 consequences).
