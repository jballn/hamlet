# ADR-0043 — TUI rooms on the SPEC §2 tree; landmark + escalation projections

Status: Accepted (supersedes ADR-0042 §4 room contents only; amends ADR-0041 cl. 4–5)
Sprint: 08-cli-tui · Prompt: 03-frontend-engineer (step 03)
Refs: SPEC §2, §7, §12.2; CONVENTIONS §2, §3; ADR-0029 cl. 4 (escalation inbox), ADR-0041, ADR-0042

## Context

ADR-0042 §4 put the vault in Market and decrees in Keep. SPEC §2 (law) puts `vault/ archives/ census/` under `keep/` and the Mayor under `keep/census/manor/`; the 08/03 prompt also maps rooms that way. ADR-0041's projections cover commissions/census/decrees/vault/mail/metrics only: register, blueprints, gossip, patrons, dispatch, skills, pkgcache, archives, Mayor wake state and the escalation inbox are invisible to a pure client, and nothing ever publishes on the WS broadcast. WS auth read a non-existent `uri` header, so `?token=` never worked (browsers cannot set WS headers).

## Decision

1. **Room graph = SPEC §2, 1:1.** Guild (`guild/`: register, blueprints, ledger + INDENTURE statuses), Tavern (`tavern/`: gossip, patrons/stasis), Courier (`courier/`: mail spools, freight, dispatch outbox/inbox/dead), Market (`market/`: skills, pkgcache), Keep (`keep/`: vault, archives, census parcels with live status/telemetry), Manor (`keep/census/manor/`: Mayor session/wake incarnation, decree queue + ack states, escalation inbox). Keys from ADR-0042 stay (`Tab`/`S-Tab`, `1-6`, `h/j/k/l`, `?`, `q`, `s`, `d`, `e`); added: `m` mail compose, `Enter` detail, `f` ticker filter, `r` refresh, mouse click on landmark tabs/rows.
2. **`GET /api/v1/landmarks`** → `LandmarksView`: per-landmark sorted name lists and counts read via `HamletTree` accessors only. Pure projection, no side effects.
3. **Escalation inbox** — `GET /api/v1/escalations` → `Vec<EscalationView>` (every `EscalationNotice` in `manor/escalations/`, keyed by notice file name, `resolved` from the record below). `POST /api/v1/escalations/:id/resolve {resolution}` writes `manor/escalations/.resolved/<file>` (`EscalationResolution`, atomic temp→fsync→rename; hidden like `decrees/.rejected`) and mails the verdict to the Mayor (UID 1000) through the existing `MailRouter`, so the human answer reaches the Mayor on the normal mail-wake plane. Notices are never moved or edited (evidence stays append-only). Re-resolving is 409.
4. **WS publication** — command handlers broadcast the outcome they just produced (`mail_sent`, `agent_steered`, `agent_hibernated`, `agent_woken`, `commission_created`, `commission_closed`, `escalation_resolved`). They report; they never trigger work. Subsystem-originated events (deliveries, decree execution, bundles, wakes, crashes) attach to the same sender in later sprints; the TUI already renders their `event_kind`s.
5. **WS auth** reads the real query string (`RawQuery`) for `?token=`; header auth unchanged.
6. **Client discipline** — `hamlet-tui` reads only `run/daemon.{endpoint,token}` (discovery, as ADR-0041/CLI) and nothing else under `.hamlet/`. No new crates: ratatui (+ its crossterm re-export), reqwest, tokio, tokio-tungstenite (sync client on a thread; avoids an unlisted `futures` dep). Goldens use ratatui `TestBackend` + committed text files (no `insta`).

## Consequences

- Every room count is a daemon-projected number; a section the API cannot see is never invented.
- Two read routes + one command route added to the ADR-0041 surface; check-deps unchanged.
- The WS ticker is honest but sparse until subsystems publish; the reconnect path performs a full snapshot refresh so no ghost data survives a daemon restart.
