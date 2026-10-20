# ADR-0042 — CLI command tree and TUI architecture

Status: Accepted
Sprint: 08-cli-tui · Prompt: 01-architect (step 01)
Refs: SPEC §2 (landmarks), §12.1 (CLI), §12.2 (TUI), §14; CONVENTIONS §2 (dependencies, purity seam); ADR-0041 (local API)

## Context

SPEC §12 requires consolidating functional verbs into an ergonomic CLI command tree and providing an interactive MUD-style TUI.
Clients must never touch village disk state directly, keeping `hamlet-core` pure and the daemon as the singular governor.
`clap`, `ratatui`, and `crossterm` were previously confined to dev-dependencies in Sprint 00. To implement the production CLI and TUI binary, they must be promoted to the runtime allowlist per CONVENTIONS §2.

## Decision

1. **Dependency Promotion**: `clap` (derive), `ratatui`, and `crossterm` are promoted from `DEV_ALLOW` to `RUNTIME_ALLOW` in `scripts/check-deps.sh`.
2. **Global Flags & Exit Codes**:
   - Flags: `--dir <PATH>` (village root, default `.`), `--json` (strict JSON stdout), `--quiet`, `--verbose`.
   - Exit codes: `0` = Success, `1` = Operational/domain error, `2` = Usage/parse error, `3` = Daemon unavailable/connection failure.
   - Errors: Default to human stderr; emit `{"error":"...","code":N}` on `--json`.
3. **CLI Command Tree & Migration Map**:
   - `hamlet init [--blueprint <NAME>]`: bootstrap village (migrated sprint 01, offline).
   - `hamlet daemon run|status|stop`: lifecycle control (migrated sprint 04/new).
   - `hamlet commission list|new <slug>|show <slug>|close <slug>`: commission state machine (migrated sprint 06).
   - `hamlet agent list|show <uid>|hibernate <uid>|wake <patron>|steer <uid> <prompt>`: agent stasis & steer (migrated sprint 03/07).
   - `hamlet decree list|show <name>|escalate`: mayor decrees & human overrides (migrated sprint 05).
   - `hamlet mail list|send <from> <to> <subj>|show <id>`: courier messaging (migrated sprint 04).
   - `hamlet vault list|export <name>`: blueprint vault (new).
   - `hamlet metrics [--live]`: system and agent telemetry (migrated sprint 03).
   - `hamlet tui [--room <NAME>]`: launches interactive terminal MUD (new).
4. **TUI Room Graph & Landmark Layout**:
   - Landmarks: Guild (commissions, active parcels), Tavern (census, stasis patrons), Courier (mail outbox/inbox), Market (vault blueprints), Keep (decrees, charter, escalations), Manor (metrics, daemon health).
   - Navigation: `Tab`/`Shift-Tab` cycles rooms; `1-6` hotkeys directly; `h/j/k/l` navigates room lists; `?` opens help; `q` quits.
   - Status & Feeds: Bottom bar renders a ticker fed by `/api/v1/events/ws`.
   - Modals: Steer (`s`), Decree review (`d`), Escalation inbox (`e`).
   - Pure View: TUI communicates exclusively over ADR-0041 localhost HTTP/WS. Zero filesystem or sled access.

## Consequences

- Full SPEC §12 surface is covered with consistent scriptable JSON output and exit code guarantees.
- Operator can observe village state and execute interventions safely in a rich terminal interface.
- CLI/TUI logic remains decoupled from internal daemon storage representations.
