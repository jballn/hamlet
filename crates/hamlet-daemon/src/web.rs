//! axum web server + WebSockets (SPEC §3, §12.3; WS schema [DEV] → ADR 09).
//!
//! Serves RPG frontend asset layers and webhook endpoints; streams
//! real-time canvas metrics and chat bubbles over tokio-tungstenite. The
//! frontend renders exclusively from daemon-pushed state — this module is
//! the only HTTP/WS surface.
//!
//! // TODO(sprint 04): axum skeleton + webhook routes.
//! // TODO(sprint 09): WS metrics/chat protocol.
