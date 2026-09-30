---
role: qa-engineer
sprint: 04
step: 5
---

# 04/05 — Adversarial comms suite

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §8, ADR-0009/0010,
HANDOFF.md sprint 04 (all steps).

Committed adversarial suite:

1. **Mail**: header injection (newlines in Subject/From), oversized bodies,
   spoofed From vs daemon-asserted identity, mail to unallocated/hiber-
   nated/crashed UIDs (bounce paths), partial-file visibility (writer crash
   mid-rename), duplicate-delivery-after-restart (must be zero), FIFO per
   recipient under 1k-message storm.
2. **Watcher**: rapid create/delete churn, events during daemon restart
   (no lost reactions for tree-state-visible events), reaction ordering
   determinism (sorted, sequenced).
3. **Freight**: world-writable dir flagged; ACL granting foreign UID
   flagged; agent tampering with quarantine flag fails (ro to agents —
   guarded podman test); promotion-read works despite strict ACLs.
4. **Dispatch**: non-allowlisted URL refused; secret material never present
   in outbox/inbox files, logs, or sled; retry schedule matches ADR-0010
   deterministic spec; dead-letter after max attempts; malformed JSON →
   precise error file, no panic.
5. **Inbound webhooks**: bad auth, oversize body, path-traversal topic
   names, content-type smuggling → all rejected with 4xx and sanitizer-
   clean error artifacts.

Findings → HANDOFF.md. No product fixes. Verdict: `GATE 04/05: PASS|FAIL`.
