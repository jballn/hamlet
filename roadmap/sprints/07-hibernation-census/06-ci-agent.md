---
role: ci-agent
sprint: 07
step: 6
---

# 07/06 — Sprint 07 gate

Precondition: HANDOFF.md shows `GATE 07/05: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; guarded podman suite; `scripts/e2e-commission.sh`
(both modes where available). Verify ADR-0013 matches implementation;
verify all SPEC §10 exit criteria are covered by named tests (list them in
the report).

PASS/FAIL table into HANDOFF.md ending `GATE 07/06: PASS|FAIL`.
