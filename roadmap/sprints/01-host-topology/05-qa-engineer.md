---
role: qa-engineer
sprint: 01
step: 5
---

# 01/05 — Adversarial suite: topology & state

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §2/§4/§11, all HANDOFF.md
entries for sprint 01.

Write and run adversarial tests (as a committed test suite, not throwaway):

1. **Sanitizer escapes**: names containing `../`, absolute paths, NUL,
   symlinks-as-names, unicode homoglyphs, 65+ chars, leading `-`. Every
   reject must produce a typed error; every accept must round-trip.
2. **Traversal at the API boundary**: attempt tree accessor calls with raw
   strings where possible; verify SafeName is genuinely required (compile-
   fail tests via `trybuild` OR documented type-level proof — record which).
3. **Charter atomicity**: kill -9 simulation mid-save (torn temp), concurrent
   savers (two threads), reload-after-crash equality; corrupted JSON →
   hard error with byte offset.
4. **UID slots**: allocate 1000 slots → exhaustion error at 1001st; hibernated
   slot not reusable; double-allocation of one identity is idempotent, not
   duplicated.
5. **Init**: re-init idempotency; init into a non-empty foreign dir refuses;
   `.env` mode is exactly 0600.
6. **Resumability**: full cycle — init, allocate, save, simulated daemon
   restart (fresh load from disk only) → identical state.

File findings (file:line, repro, expected vs actual, SPEC citation) into
HANDOFF.md. Do NOT fix product code. End with a verdict line:
`GATE 01/05: PASS` or `GATE 01/05: FAIL` plus findings list.
