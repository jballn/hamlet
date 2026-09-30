# Sprint 05 — The Mayor (Executive Control Loop)

**Goal:** The stateless executive is real: event-driven wakes, pristine
single-turn context reconstitution (MAYOR.md + charter.json + event
artifact), decrees emitted as files, daemon-side decree parsing/execution,
zero-token sleep between events.

**Scope boundary:** Commission decomposition *content* is Sprint 06; this
sprint builds the wake → packet → decree → execute machinery and the
provision/hibernate/steer/escalate decree paths that don't depend on it.

**Exit criteria (SPEC §7):**
- Decree vocabulary + file encodings resolved (ADR-0011): atomic write,
  acknowledgment, idempotent replay protection.
- Mayor session lifecycle: manor parcel (UID 1000) provisioned via Sprint 02;
  sleeps at zero token cost; woken only by events.
- Context packet builder: MAYOR.md + charter state grid + targeted event
  artifact, single-turn, pristine (no accumulated thread).
- Decree executor: provision-agent, hibernate-agent (hook to Sprint 07),
  reassign-task, steer-session, escalate-to-human, approve/reject-promotion
  (hook to Sprint 06) — idempotent, charter-updating, TITLE-stamped.
- Wake triggers wired from Sprint 04 reactions: mail-to-mayor, webhook
  events, freight violations, decree ack timeouts.
- End-to-end (FakeAgent mayor): event → wake → packet → decree file →
  execution → ack → sleep, fully observable from the tree.
- Branch merged, tag `sprint/05-mayor-loop`.

## Prompt sequence
1. `01-architect.md` — decree vocabulary + encodings (ADR-0011)
2. `02-daemon-engineer.md` — mayor session lifecycle + context packets
3. `03-daemon-engineer.md` — decree executor + wake triggers
4. `04-qa-engineer.md` — adversarial mayor suite
5. `05-ci-agent.md` — gate
6. `06-commit-agent.md` — merge & tag
