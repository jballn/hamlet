---
role: frontend-engineer
sprint: 08
step: 3
---

# 08/03 — MUD-style TUI village

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.2, ADR-0014 (rooms IA +
API), HANDOFF.md (steps 1–2).

## Deliverable — `hamlet-tui` (`hamlet tui` or standalone `hamlet-tui` bin)

ratatui + crossterm, rendering EXCLUSIVELY from the local API (projections +
WS event stream). No `.hamlet/` reads, no orchestration logic.

- **Rooms** per ADR-0014 graph: Guild (register, blueprints, ledger board of
  commissions + indenture statuses), Tavern (gossip feed, patron stasis
  list), Courier (mail spools, dispatch outbox/inbox), Market (skills,
  pkgcache stats), Keep (vault, archives, census — parcel rooms with live
  agent status/telemetry), Manor (Mayor: wake log, decree queue, ack states,
  escalation inbox).
- Navigation: vim keys + tab room-switcher + clickable landmarks (mouse
  capture); room descriptions flavored MUD-style but state-honest (counts
  and statuses are real).
- Live streams: WS-fed event ticker (mail delivered, decrees executed,
  bundles applied, wakes, crashes) with per-room filtering.
- Intervention surfaces: steer dialog (compose interrupt → agent), decree
  review (show + ack states), escalation inbox (read + resolve), mail
  compose. Every mutating action goes through API command endpoints.
- Reconnect: daemon restart → exponential-backoff reconnect, full state
  refresh, no ghost data.

Verify: unit tests on view-model mapping (API projections → widgets, pure);
scripted demo mode (`--demo` fixture stream) so CI can smoke-render frames
headlessly (insta-style frame goldens). Finish: HANDOFF.md. Commit:
`feat(tui): MUD-style village over local API (SPEC §12.2)`.
