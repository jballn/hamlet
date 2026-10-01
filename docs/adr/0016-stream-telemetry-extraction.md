# ADR-0016 — Stream metric extraction: spike definition & usage accounting

Status: Accepted
Date: Sprint 03 (step 03/04)
Refs: SPEC §6, §11.2, §14 [DEV]; CONVENTIONS §3 r4, §6; ADR-0005/0015
Supersedes: —

## Context

SPEC §6 makes the daemon the only source of session metrics ("Agents do not
self-report"); step 03's HANDOFF left the latency-spike threshold undecided.
pi's `message_update.usage` is a **cumulative** snapshot for the in-flight
assistant response (pi docs `json.md`), so summing frames would multiply-count
every stream. And a malformed frame may echo mail text or a secret, so *what a
frame records* is policy, not an implementation detail.

## Decision

1. **Latency spike** = a stream span measured between frame *reads* whose
   duration is `>= sessions.latency_spike_ms` (config **v4**, default 30 000,
   range 1..=3 600 000; additive key — v1–v3 documents still parse). Closed
   operation vocabulary: `agent-run` (agent_start→agent_end), `agent-task`
   (first agent_start→agent_settled, spanning continuation runs), `agent-turn`
   (turn_start→turn_end), `tool-call <name>` (tool_execution_start→_end matched
   by `toolCallId`), `stream-gap` (a frame gap while a task is in flight **and
   no tool span is open** — idle time never spikes; a slow tool is attributed to
   its own span only), and `delivery-ack`/`interrupt-ack` (a wire id this daemon
   wrote → its `response`). Spans still open at stream end are closed and
   evaluated, so a session killed mid-task records the span it died in.
2. **Tokens.** Snapshots are differenced against a per-message baseline;
   `message_end.message.usage` is authoritative and closes the message; a
   snapshot whose `totalTokens` regressed rebases the baseline (a new message
   opened without a boundary frame), so counters can never underflow or stall.
   Only `input`/`output` reach `tokens.v1` — ADR-0005 cl. 5 forbids silently
   reshaping a `.v1` record — while cache/reasoning/`totalTokens` are counted in
   `StreamStats` and reported in `session-stream-summary`.
3. **Cost.** USD floats become integer micro-USD: round half away from zero,
   `cost.total` when finite and positive else the component sum, NaN/negative →
   0, saturating at `u64::MAX`. Cumulative snapshots are differenced *after*
   conversion, so no float drift accumulates across a stream.
4. **Malformed frames** are counted and skipped, never fatal, and their content
   is never recorded (CONVENTIONS §6) — only the parser's reason and the frame's
   byte length. Agent-supplied strings (tool names, record types) are scrubbed
   to printable ASCII and capped at 64 characters.

## Consequences

- `config.json` v4 and the core/CLI goldens changed in-commit; one knob drives
  every span family.
- Spans are measured at read time, so a scripted delay can read back a few ms
  short — tests assert the threshold breach, not the exact delay.
- A new metric means a new `StreamStats` counter + summary field, or an additive
  `.v1` field / new tree generation (ADR-0005 cl. 5).
