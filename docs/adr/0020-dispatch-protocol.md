# 0020 — Dispatch protocol: outbox/inbox schemas, allowlist, retry policy

Status: Accepted (Sprint 04 prompts cite this as "ADR-0010"; that number was
claimed by the podman invocation policy — see README renumbering table.
Resolves SPEC §8.4 retry/backoff `[DEV]`)

## Context

SPEC §8.4: JSON files in `courier/dispatch/outbox/` are POSTed via reqwest;
responses land in `inbox/`; endpoint auth via `.env`; retry/backoff `[DEV]`.
The state contract demands a reproducible (jitter-free) schedule and
file-as-truth artifacts; secrets never appear in tree files, sled, or logs.

## Decision

1. **Outbox** `courier/dispatch/outbox/<correlation>.json`
   (`dispatch::OutboxRequest`): canonical pretty JSON, alphabetical keys,
   `deny_unknown_fields` — `dispatch_version: 1`, `correlation` (reuses
   `session::CorrelationId`; dispatch form `d-<seq>-<uid>.<inc>`), `method`
   (`get|post|put|patch|delete`), `url` (**https only**, RFC 3986 charset,
   no credentials, ≤2048), `headers_env` (`BTreeMap<header-name, ENV_KEY>`;
   names are HTTP tokens, keys `[A-Z][A-Z0-9_]{0,63}`; values never inline),
   `body` (arbitrary JSON). File ≤ 1 MiB.
2. **Inbox** `courier/dispatch/inbox/<correlation>.json` (`InboxResponse`):
   `dispatch_version`, echoed `correlation`, 1-based `attempt`, `outcome`
   tagged by `kind`: `http` {status, headers, body} or `error` {error:
   opaque ASCII ≤128}. Stored headers lowercased, capped (32 × 128/512),
   redacted (`authorization`, `proxy-authorization`, `cookie`, `set-cookie`,
   `x-api-key` never stored); body ≤64 KiB with truncation marker.
3. **Allowlist**: `config.json` `dispatch.url_allowlist` entries
   `https://host[:port][/prefix]`; match = exact host (case-insensitive) +
   exact port + byte-prefix path. Empty list denies all; `http://` never.
4. **Retry**: deterministic bounded exponential, no jitter — delay before
   attempt n ≥ 2 = `min(backoff_initial_ms · 2^(n−2), backoff_max_ms)`.
   Defaults: 5 000 ms / 900 000 ms / `max_attempts` 5 (config v5 keys).
   Retryable: transport errors, HTTP ≥ 500, 429. Other 4xx = permanent
   delivered response (inbox written, no retry, no dead-letter). Validation
   failures (schema, allowlist, missing env key) → error inbox + immediate
   dead-letter, zero network attempts.
5. **Dead-letter**: after the final retryable failure the outbox file is
   renamed into `courier/dispatch/dead/` (static, `ensure`d) + final error
   inbox file. Tree-visible, operator-recoverable, never daemon-deleted.
6. **Secrets**: resolved from `.env` by key name at send time only; a
   referenced-but-missing key is a validation failure (cl. 4), never an
   empty-header send.

## Consequences

Every round-trip is reconstructible from tree artifacts and the schedule is
a pure function QA asserts tick-by-tick. `DispatchConfig` gains v5 keys
additively (`serde(default)`); secret hygiene is structural.
