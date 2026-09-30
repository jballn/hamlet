# Sprint 04 — Communication Planes

**Goal:** All four communication planes live: inotify-driven mail spool
routing into session stdin, freight POSIX ACL sandboxes, dispatch outbox/
inbox webhooks, and axum inbound webhook endpoints.

**Scope boundary:** No Mayor decision logic (Sprint 05 consumes these
planes). Mail *delivery pipe* exists from Sprint 03 — this sprint builds
routing, format, and watchers.

**Exit criteria (SPEC §8, §4):**
- Watcher core: notify-based, debounced, deterministic event → reaction
  dispatch (ADR-0009).
- Mail: header format [DEV resolved], spool routing per-UID, sequential
  delivery into Sprint 03 queues, bounce on overflow, cross-UID read
  blocked by perms (tested).
- Freight: daemon-verified ACL sandbox creation (`setfacl` in-container
  path), recipient-UID-scoped directories; ACL audit tool.
- Dispatch: outbox JSON → reqwest POST (rustls), inbox response files,
  retry/backoff policy (ADR-0010), secrets only from `.env`.
- axum: inbound webhook endpoints writing validated dispatch-inbox files /
  mail; sanitizer on every derived filename.
- Branch merged, tag `sprint/04-communication`.

## Prompt sequence
1. `01-architect.md` — mail format + watcher/dispatch protocols (ADR-0009/0010)
2. `02-daemon-engineer.md` — watcher core + mail routing
3. `03-container-engineer.md` — freight ACL sandboxes
4. `04-daemon-engineer.md` — dispatch outbox/inbox + axum inbound webhooks
5. `05-qa-engineer.md` — adversarial comms suite
6. `06-ci-agent.md` — gate
7. `07-commit-agent.md` — merge & tag
