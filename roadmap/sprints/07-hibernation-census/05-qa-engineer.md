---
role: qa-engineer
sprint: 07
step: 5
---

# 07/05 — Adversarial stasis suite

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §10, ADR-0013, HANDOFF.md
sprint 07 (all steps).

Committed adversarial suite:

1. **TITLE integrity**: agent-side TITLE.md edits detected; daemon never
   rewrites history (append-only assertion over full lifecycle); golden
   sequence stability across restarts.
2. **Hibernation races**: hibernate while a task is in-flight (graceful
   drain), hibernate during interrupt, double-hibernate decree (idempotent),
   hibernate a crashed session (allowed, marked), kill -9 mid-copy →
   pre-hibernation state OR crashed-hibernating, never half-stasis.
3. **Stasis integrity**: corrupt one file in home.uid.bak → awaken refuses
   (manifest mismatch) with typed error; patron PROPRIETOR frontmatter
   tampering → rejected at awaken.
4. **Awakening**: awaken into full slot space → hard error; awakened agent
   cannot read its old parcel (released); pkgcache counters prove the fast
   path; two awakens of one patron → two independent identities, shared
   lineage refs.
5. **Convergence independence**: blueprint promotion failure doesn't block
   patron save and vice versa; both failures → escalation event exactly once.
6. **Resource policy**: daemon NEVER auto-hibernates (audit all hibernate
   call sites: trigger must be decree|operator — static test over the code
   path + runtime assertion); pressure suggestions carry evidence refs.
7. **Full-cycle resumability**: provision → work → hibernate → restart
   daemon → awaken → converge → close, with kills at 3 random points
   (deterministic seeds) → final tree state identical to the no-kill run.

Findings → HANDOFF.md. No product fixes. Verdict: `GATE 07/05: PASS|FAIL`.
