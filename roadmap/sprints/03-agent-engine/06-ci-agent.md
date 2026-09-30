---
role: ci-agent
sprint: 03
step: 6
---

# 03/06 — Sprint 03 gate

Precondition: HANDOFF.md shows `GATE 03/05: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; guarded podman suite if available. Verify
ADR-0008 matches the implemented wire format (spot-check golden envelopes).
Verify FakeAgent is committed and used by the suite (no test depends on a
live pi binary unguarded).

PASS/FAIL table into HANDOFF.md ending `GATE 03/06: PASS|FAIL`.
