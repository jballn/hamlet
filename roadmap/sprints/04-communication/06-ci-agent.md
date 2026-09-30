---
role: ci-agent
sprint: 04
step: 6
---

# 04/06 — Sprint 04 gate

Precondition: HANDOFF.md shows `GATE 04/05: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; guarded podman suite if available. Verify
ADR-0009/0010 match implementation (golden vectors). Verify the default
blueprint version bump is committed and parses.

PASS/FAIL table into HANDOFF.md ending `GATE 04/06: PASS|FAIL`.
