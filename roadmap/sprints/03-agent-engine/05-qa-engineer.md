---
role: qa-engineer
sprint: 03
step: 5
---

# 03/05 — Mock-agent adversarial suite

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §6, ADR-0008, HANDOFF.md
sprint 03 (all steps).

Committed adversarial suite using FakeAgent (no podman required):

1. **Ordering**: 100 enqueued messages arrive in FIFO order, one per task
   boundary; no interleaving under a chatty/slow agent.
2. **Interrupt races**: steer during drain-write, steer while idle, double
   steer, steer during crash-restart window — framing never corrupts;
   correlation ids always resolvable.
3. **Crash paths**: agent crashes mid-task with queued messages → restart
   policy honored, queue preserved, charter status transitions correct,
   TITLE.md stamped, sled trace present; restart-limit exhaustion → crashed
   terminal state, no zombie respawn loop.
4. **Restart recovery**: kill daemon mid-delivery → restart → undelivered
   queue recovered, no duplicate delivery of an acknowledged message.
5. **Malformed streams**: garbage frames, truncated JSON, hostile huge
   frames, fake "metric" frames that try to smuggle charter-shaped state →
   skipped/traced, never applied.
6. **Secrets hygiene**: assert `.env` material never appears in FakeAgent-
   visible files, recorded argv, charter, or sled.
7. **Backpressure**: overflow → typed bounce, no unbounded memory growth.

Findings → HANDOFF.md (file:line, repro, expected vs actual, SPEC cite).
No product fixes. Verdict: `GATE 03/05: PASS|FAIL`.
