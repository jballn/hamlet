# Sprint 06 — Commission Lifecycle

**Goal:** Multi-tenant project management: commissions as independent world
slices with bare git repos, Mayor-driven INDENTURE decomposition, agent work
branches, DEED→blueprint promotion, and the vault deliverable loop.

**Scope boundary:** Hibernation at commission end is Sprint 07 (this sprint
triggers the hooks). RPG/TUI surfaces are later; CLI gets functional
commands now, polished in Sprint 08.

**Exit criteria (SPEC §9, §7, §5):**
- `hamlet commission new` creates commission<n>/ (sanitized slug), bare git
  repo, COMMISSION.md from operator input; charter state tracks it; Mayor
  wake fires (CommissionCreated).
- INDENTURE flow: Mayor decomposition decrees produce per-agent bounded
  contracts; agents receive their entries via mail/steer; charter tracks
  entry status.
- Git work branches: ro-mount-compatible push mediation resolved
  (ADR-0012) and tested; daemon-mediated history integrity.
- DEED promotion: `### Dynamic Alterations` parsed at completion →
  validated → written back into parent blueprints (version bump); malicious/
  malformed blocks rejected.
- Vault promotion: Mayor ApprovePromotion → daemon sanitizes name, copies
  freight artifact → `keep/vault/`, session spin-down triggered, archives
  telemetry export written.
- E2E script: `scripts/e2e-commission.sh` — init → commission → fake agents
  (work branch + DEED + freight deliverable) → mayor approval → vault.
- Branch merged, tag `sprint/06-commission-lifecycle`.

## Prompt sequence
1. `01-architect.md` — commission state machine + git mediation (ADR-0012)
2. `02-daemon-engineer.md` — commission creation + bare repos + CLI basics
3. `03-daemon-engineer.md` — INDENTURE decomposition flow
4. `04-runtime-engineer.md` — agent work-branch loop (push mediation impl)
5. `05-daemon-engineer.md` — DEED → blueprint promotion
6. `06-daemon-engineer.md` — vault promotion + spin-down + archives export
7. `07-qa-engineer.md` — adversarial lifecycle suite + e2e script
8. `08-ci-agent.md` — gate
   then `09-commit-agent.md` — merge & tag
