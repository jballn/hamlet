# 0022 — Inbound webhook endpoints: auth, routing table, event envelope

Status: Accepted (resolves SPEC §8.5 `[DEV]` surface: auth shape, topic
routing, inbox envelope for daemon-originated events)

## Context

SPEC §8.5: axum endpoints accept external events, write them as dispatch
inbox files / mail, and wake the Mayor on decree-relevant triggers. The
listen address exists (`web_listen_addr`, config v1) but auth, body policy,
the topic → sink routing table, and the file envelope for an *inbound*
event (ADR-0020's inbox schema describes *responses*) were open.

## Decision

1. **Auth**: shared secret in `.env` under `webhooks.auth_env_key`
   (default `HAMLET_WEBHOOK_AUTH`), presented as the `X-Hamlet-Auth`
   request header; comparison is constant-time-ish (length fold +
   byte-wise XOR accumulate, no early exit). Missing/empty `.env` key
   fails **closed** (503) — a misconfigured village never accepts
   unauthenticated events. Bad/absent header → 401.
2. **Body policy**: `webhooks.max_body_bytes` (default 64 KiB, cap
   1 MiB) enforced at the hyper level by tower-http's
   `RequestBodyLimitLayer` (oversize → 413 before any handler runs —
   axum's `DefaultBodyLimit` does not apply to raw `Bytes` extraction in
   axum 0.7) and
   re-checked before parsing; the body must be a JSON value (400 else).
   Endpoint shape is `POST /webhook/<topic>`; `<topic>` must pass the §4
   sanitizer **and** name a configured route — everything else is 404
   (invalid, unknown, and traversal-shaped topics are one indistinguishable
   class; no path ever reaches the disk unsanitized).
3. **Routing table** (config v6, additive `webhooks` block):
   `routes: { <topic>: { dispatch_inbox: bool, mail_to: [slot uid…] } }`,
   ≥1 sink per route; empty table = reject-all. `webhooks.enabled=false`
   (default) means the server does not start at all.
4. **Inbox envelope**: a routed event with `dispatch_inbox` is written as
   `courier/dispatch/inbox/<correlation>.json` — an ADR-0020
   `InboxResponse` with `attempt: 1`, `kind: "http"`, **`status: 202`**
   (the marker distinguishing daemon-accepted inbound events from real
   upstream responses), empty headers, `body` = canonical (sorted-key)
   JSON of the validated event (the ADR-0020 cl. 2 64 KiB inbox body cap
   and truncation marker apply — the constructor is the only write path).
   Correlation `d-<seq>-1000.0`: seq from
   the durable sled correlation counter of `AgentKey{commission:0,
   uid:1000}` (monotonic across restarts, no wall-clock input).
5. **Mail sink**: `MailRouter::send` with `MailActor::Patron("webhook")`
   (daemon-asserted `From:` — the endpoint cannot claim an identity),
   subject `webhook: <topic>`, body = canonical event JSON truncated at
   the 64 KiB mail cap. Normal bounce rules apply (ADR-0019 cl. 4).
6. **Mayor wake**: every accepted event fires the `WebhookWake` hook
   (fire-and-forget seam; stderr recorder until Sprint 05 consumes it)
   and one sled trace (`webhook-inbound`: topic + correlation + sinks,
   never body bytes). Handlers contain no orchestration: validate →
   write files → emit hook. The tree remains the truth.

## Dependencies

`tower-http` (feature `limit`) joins the SPEC §3 runtime allowlist via
this ADR: its hyper-level `RequestBodyLimitLayer` is the 413 enforcer —
axum's own `DefaultBodyLimit` does not apply to the raw `Bytes`
extractor in axum 0.7 (verified by test), so the cap must live below the
extractors. No other new dependencies (`reqwest`, `axum`, `dotenvy` are
already allowlisted; SPEC §3).

## Consequences

Accepted events are fully reconstructible from tree artifacts (inbox
file + spool mail) with end-to-end correlation. Secrets live only in
`.env`; a 401/404/413/400 rejection writes nothing to the tree.
