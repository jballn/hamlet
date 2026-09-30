---
role: qa-engineer
sprint: 08
step: 4
---

# 08/04 — Interface suite + scripted demo e2e

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12, ADR-0014, HANDOFF.md
sprint 08 (all steps).

Committed suite:

1. **API boundary**: TUI/CLI contain zero direct filesystem access to a
   live tree (static grep test over both crates for tree path usage);
   every mutating verb maps to an API command endpoint (route coverage
   table).
2. **CLI matrix**: every verb × {human, --json} × {success, typed failure}
   goldens; exit-code policy honored; daemon-down errors are clean.
3. **Auth**: local API token enforced; foreign-port requests without token
   rejected; token file 0600.
4. **TUI honesty**: demo-fixture renders match goldens; WS event stream
   order preserved in ticker; reconnect-after-daemon-kill shows fresh state,
   no ghosts (scripted test).
5. **e2e demo script** `scripts/e2e-village-demo.sh`: init → daemon → TUI
   demo-mode frame capture → CLI verbs cycle → commission flow via CLI
   (reusing e2e-commission fake agents) → vault list shows artifact.
6. **Keyboard operability**: every TUI action reachable without mouse
   (documented keymap + test of key bindings table).

Findings → HANDOFF.md. No product fixes. Verdict: `GATE 08/04: PASS|FAIL`.
