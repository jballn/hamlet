---
role: daemon-engineer
sprint: 07
step: 3
---

# 07/03 — Hibernation routine + patron stasis

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §10 (hibernation routine),
ADR-0013, HANDOFF.md (steps 1–2; sprint 03 graceful-stop API; sprint 05
HibernateAgent decree seam).

## Deliverable — implement the `Stasis` trait (sprint 05 hook)

`hibernate(uid, trigger)`:
1. drain delivery queue (undelivered mail preserved per Sprint 03
   persistence) → graceful stop (Sprint 03) → container teardown (Sprint 02);
2. copy parcel `home/` → `tavern/patrons/PROPRIETOR<name>/home.uid.bak/`
   with manifest checksums (ADR-0013); copy size guards + atomic staging
   (build in temp, verify, rename);
3. serialize the mutated agent profile: read parcel PROPRIETOR.md →
   validate frontmatter → write patron PROPRIETOR.md (intelligence-layer
   promotion — independent of DEED/blueprint promotion);
4. charter: slot status → hibernated (mapping persists); census TITLE entry;
   sled trace; compute freed (no container/process remains — assert);
5. parcel dir disposition: archive metadata (TITLE/DEED/PROPRIETOR copies)
   to `keep/archives/`, then release the parcel dir (home is in stasis).

Triggers: Mayor decree, operator CLI (`hamlet agent hibernate <uid>`),
never daemon-autonomous (ADR-0013). Failure at any step → rollback to the
pre-hibernation live state OR (if teardown already happened) a `crashed-
hibernating` charter state with full TITLE record — decide + record the
matrix in HANDOFF.

Verify: FakeAgent tests: clean hibernate, rollback paths, queue preservation,
manifest integrity, no orphan processes. Finish: HANDOFF.md. Commit:
`feat(daemon): hibernation routine + patron stasis (SPEC §10)`.
