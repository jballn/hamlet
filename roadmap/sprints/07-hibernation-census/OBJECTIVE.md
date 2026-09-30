# Sprint 07 — Hibernation & Census

**Goal:** The stasis pool and tenant leasing planes are complete: TITLE
chain-of-title, hibernation routine, quick awakening, dual-state convergence
at commission end, and resource-bounds policy.

**Scope boundary:** No UI beyond functional CLI. This closes the daemon
governor's lifecycle story (SPEC §10) — after this sprint every plane in
SPEC §1–§11 exists.

**Exit criteria (SPEC §10, §4):**
- TITLE.md: append-only daemon-stamped chain-of-title (trigger lineage,
  session incarnations, crashes, promotions, hibernations).
- Hibernation routine: graceful stop → home copied to
  `tavern/patrons/PROPRIETOR<name>/home.uid.bak/` → mutated PROPRIETOR.md
  serialized → compute freed, UID slot mapping persists in charter.
- Quick awakening: patron match → next available UID → home dropped in →
  session live, skipping clean bootstrap (measurably faster; asserted).
- Dual-state convergence at commission end: DEED → blueprints (Sprint 06
  machinery) AND PROPRIETOR → patrons, as two independent promotions.
- Resource bounds: parcel limits from config (cpu/mem/pids via podman
  flags), pressure-trigger hibernation candidates surfaced to the Mayor
  (decree stays Mayor-authored — daemon suggests, never auto-hibernates
  agents mid-task; policy ADR-0013).
- `hamlet agent list|hibernate|wake` functional CLI.
- Restart-resumability across hibernate/awaken cycles.
- Branch merged, tag `sprint/07-hibernation-census`.

## Prompt sequence
1. `01-architect.md` — stasis formats + resource policy (ADR-0013)
2. `02-daemon-engineer.md` — TITLE chain-of-title + census records
3. `03-daemon-engineer.md` — hibernation routine + patron stasis
4. `04-daemon-engineer.md` — quick awakening + dual-state convergence
5. `05-qa-engineer.md` — adversarial stasis suite
6. `06-ci-agent.md` — gate
7. `07-commit-agent.md` — merge & tag
