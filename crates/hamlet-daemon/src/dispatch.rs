//! External webhook dispatch & inbound hooks (SPEC §8.4–8.5).
//!
//! POSTs JSON files dropped in `courier/dispatch/outbox/` via reqwest;
//! responses land in `inbox/`. Inbound axum endpoints accept external
//! events, write them as dispatch inbox files / mail, and wake the Mayor on
//! decree-relevant triggers. Endpoint auth via `.env`; retry/backoff policy
//! is [DEV] → ADR (sprint 04).
//!
//! // TODO(sprint 04)
