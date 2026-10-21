//! Daemon local HTTP+WS API router and handlers (SPEC §12.1–§12.2; ADR-0041).
//!
//! Provides the local REST API and WebSocket event stream consumed by
//! `hamlet-cli`, `hamlet-tui`, and future frontends.
//!
//! # Architecture Invariants (CONVENTIONS §2, §3; ADR-0041)
//!
//! 1. **Localhost Only**: Binds strictly to `127.0.0.1`.
//! 2. **Authentication**: Local token passed via `Authorization: Bearer <token>`
//!    or `x-hamlet-local-token`. Validated via constant-time comparison.
//! 3. **Zero Orchestration in Handlers**: Handlers only project state or delegate
//!    to existing daemon subsystems (census, ledger, supervisor, mailroute, vault).
//! 4. **Canonical Responses**: All success and error responses conform to `hamlet-core::api` schemas.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hamlet_core::api::*;
use hamlet_core::charter::{Charter, DecreeStatus};
use hamlet_core::commission::IndentureEntryStatus;
use hamlet_core::decree::{DecreeId, EscalationNotice};
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::session::{InterruptPriority, InterruptSource};
use hamlet_core::tree::HamletTree;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::census::{list_census, ProductionStasis};
use crate::ledger::Ledger;
use crate::mailroute::{MailActor, MailRouter};
use crate::supervise::Supervisor;
use crate::telemetry::{AgentKey, Telemetry};

// ---------------------------------------------------------------------------
// Shared State
// ---------------------------------------------------------------------------

/// Shared server state for local API handlers.
pub struct LocalApiState {
    /// Target village tree.
    pub tree: Arc<HamletTree>,
    /// Secret authentication token.
    pub token: LocalToken,
    /// Graceful shutdown trigger.
    pub shutdown_tx: watch::Sender<bool>,
    /// WebSocket event broadcast sender.
    pub ws_broadcast: broadcast::Sender<WsEvent>,
    /// Optional supervisor reference.
    pub supervisor: Option<Arc<Supervisor>>,
    /// Optional mail router reference.
    pub mail_router: Option<Arc<MailRouter>>,
    /// Optional stasis governor reference.
    pub stasis: Option<Arc<ProductionStasis>>,
    /// Optional commission ledger reference.
    pub ledger: Option<Arc<Ledger>>,
    /// Process start timestamp in unix ms.
    pub started_at_unix_ms: u64,
    /// Monotonic WS event sequence for events published by this API.
    ws_seq: AtomicU64,
    /// WebSocket streaming hub (ADR-0044).
    pub ws_hub: Arc<crate::web::WsHub>,
    /// Static web asset server (ADR-0044).
    pub asset_server: Arc<crate::web::AssetServer>,
}

impl LocalApiState {
    /// Constructs a minimal local API state.
    #[must_use]
    pub fn new(tree: Arc<HamletTree>, token: LocalToken, shutdown_tx: watch::Sender<bool>) -> Self {
        let (ws_broadcast, _) = broadcast::channel(256);
        let ws_hub = Arc::new(crate::web::WsHub::new(
            Arc::clone(&tree),
            token.clone(),
            shutdown_tx.clone(),
        ));
        let asset_server = Arc::new(crate::web::AssetServer::new(
            crate::web::AssetConfig::default(),
        ));
        Self {
            tree,
            token,
            shutdown_tx,
            ws_broadcast,
            supervisor: None,
            mail_router: None,
            stasis: None,
            ledger: None,
            started_at_unix_ms: unix_now_ms(),
            ws_seq: AtomicU64::new(0),
            ws_hub,
            asset_server,
        }
    }

    /// Sets the supervisor.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Arc<Supervisor>) -> Self {
        self.ws_hub = Arc::new(
            (*self.ws_hub)
                .clone()
                .with_supervisor(Arc::clone(&supervisor)),
        );
        self.supervisor = Some(supervisor);
        self
    }

    /// Sets the mail router.
    #[must_use]
    pub fn with_mail_router(mut self, mail_router: Arc<MailRouter>) -> Self {
        self.ws_hub = Arc::new(
            (*self.ws_hub)
                .clone()
                .with_mail_router(Arc::clone(&mail_router)),
        );
        self.mail_router = Some(mail_router);
        self
    }

    /// Sets the stasis engine.
    #[must_use]
    pub fn with_stasis(mut self, stasis: Arc<ProductionStasis>) -> Self {
        self.ws_hub = Arc::new((*self.ws_hub).clone().with_stasis(Arc::clone(&stasis)));
        self.stasis = Some(stasis);
        self
    }

    /// Sets the commission ledger.
    #[must_use]
    pub fn with_ledger(mut self, ledger: Arc<Ledger>) -> Self {
        self.ws_hub = Arc::new((*self.ws_hub).clone().with_ledger(Arc::clone(&ledger)));
        self.ledger = Some(ledger);
        self
    }

    /// Sets the WebSocket streaming hub.
    #[must_use]
    pub fn with_ws_hub(mut self, ws_hub: Arc<crate::web::WsHub>) -> Self {
        self.ws_hub = ws_hub;
        self
    }

    /// Sets the static asset server.
    #[must_use]
    pub fn with_asset_server(mut self, asset_server: Arc<crate::web::AssetServer>) -> Self {
        self.asset_server = asset_server;
        self
    }

    /// Broadcasts an event to connected WebSocket clients.
    pub fn broadcast(&self, event: WsEvent) {
        let _ = self.ws_broadcast.send(event);
    }

    /// Publishes the outcome a command handler just produced (ADR-0043
    /// cl. 4). Reporting only: nothing subscribes to this to trigger work.
    pub fn publish(&self, topic: WsTopic, event_kind: &str, payload: serde_json::Value) {
        let seq = self.ws_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = WsEvent::new(topic, event_kind, seq, unix_now_ms(), payload);
        let _ = self.ws_broadcast.send(event.clone());
        self.ws_hub
            .broadcast_frame(crate::web::WsBroadcastFrame::LegacyEvent(event));
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Helper to authenticate requests against the local token.
///
/// Returns `None` on success, or `Some(Response)` on unauthorized error.
fn authenticate(
    headers: &HeaderMap,
    query: Option<&str>,
    state: &LocalApiState,
) -> Option<Response> {
    // 1. Check Authorization: Bearer <token>
    if let Some(auth_val) = headers.get(AUTH_HEADER).and_then(|v| v.to_str().ok()) {
        if let Some(token_str) = auth_val.strip_prefix(BEARER_PREFIX) {
            if state.token.constant_time_eq(token_str.trim()) {
                return None;
            }
        }
    }

    // 2. Check X-Hamlet-Local-Token: <token>
    if let Some(token_val) = headers
        .get(LOCAL_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        if state.token.constant_time_eq(token_val.trim()) {
            return None;
        }
    }

    // 3. Check query param for WebSocket upgrade (e.g. ?token=<token>)
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "token" && state.token.constant_time_eq(v.trim()) {
                    return None;
                }
            }
        }
    }

    let err = ApiErrorResponse::unauthorized("invalid or missing local auth token");
    Some((StatusCode::UNAUTHORIZED, Json(err)).into_response())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/v1/daemon/status`
async fn status_handler(State(state): State<Arc<LocalApiState>>, headers: HeaderMap) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let uptime = (unix_now_ms().saturating_sub(state.started_at_unix_ms)) / 1000;
    let charter = match Charter::load(&state.tree.charter()) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(format!(
                    "failed to load charter: {e}"
                ))),
            )
                .into_response();
        }
    };

    let active_agents = charter
        .slots
        .values()
        .filter(|s| s.status == hamlet_core::slots::SlotStatus::Active)
        .count();
    let commissions_count = charter.commissions.len();
    let queued_decrees = charter
        .decrees
        .values()
        .filter(|status| **status == DecreeStatus::Queued)
        .count();

    let mut queued_mail = 0;
    if let Ok(entries) = fs::read_dir(state.tree.courier_queues()) {
        queued_mail += entries.flatten().count();
    }
    if let Ok(entries) = fs::read_dir(state.tree.courier_mail()) {
        for recipient in entries.flatten() {
            if let Ok(mail_entries) = fs::read_dir(recipient.path()) {
                for m in mail_entries.flatten() {
                    if m.path().extension().and_then(|e| e.to_str()) == Some("mail") {
                        queued_mail += 1;
                    }
                }
            }
        }
    }

    let resp = DaemonStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        uptime_secs: uptime,
        village_root: state.tree.root().display().to_string(),
        active_agents,
        commissions_count,
        queued_decrees,
        queued_mail,
    };

    (StatusCode::OK, Json(resp)).into_response()
}

/// `POST /api/v1/daemon/stop`
async fn stop_handler(State(state): State<Arc<LocalApiState>>, headers: HeaderMap) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let _ = state.shutdown_tx.send(true);
    let resp = DaemonStopResponse {
        stopping: true,
        pid: std::process::id(),
    };
    (StatusCode::OK, Json(resp)).into_response()
}

#[derive(Debug, Deserialize)]
struct CensusQuery {
    commission: Option<String>,
}

/// Pure projection of census records.
pub(crate) fn project_census(
    tree: &HamletTree,
    telemetry: Option<&Telemetry>,
) -> Vec<CensusRecordView> {
    match list_census(tree, telemetry) {
        Ok(views) => views
            .into_iter()
            .map(|v| CensusRecordView {
                uid: v.uid,
                identity: v.identity,
                commission: v.commission,
                commission_slug: v.commission_slug,
                state: v.state,
                stasis: v.stasis,
                session_count: v.session_count,
                counters: ApiCensusCounters {
                    input_tokens: v.counters.input_tokens,
                    output_tokens: v.counters.output_tokens,
                    cost_micro_usd: v.counters.cost_micro_usd,
                    traces: v.counters.traces,
                },
                last_event: v.last_event,
                last_event_ts: v.last_event_ts,
                title_digest: v.title_digest,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Pure projection of commissions summaries.
pub(crate) fn project_commissions(
    ledger: Option<&Ledger>,
    tree: &HamletTree,
) -> Vec<CommissionSummaryView> {
    let ledger_fallback;
    let l_ref = match ledger {
        Some(l) => l,
        None => {
            ledger_fallback = Ledger::new((*tree).clone());
            &ledger_fallback
        }
    };
    match l_ref.list_commissions() {
        Ok(items) => items
            .into_iter()
            .map(|item| CommissionSummaryView {
                commission_id: item.id,
                slug: item.slug,
                state: item.state.kind_name().to_string(),
                roster: item.roster.into_iter().collect(),
                deliverables_count: 0,
                brief: item.has_brief,
                indenture: item.has_indenture,
                git: if item.has_bare_git {
                    "bare".to_string()
                } else {
                    "-".to_string()
                },
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `GET /api/v1/census`
async fn census_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    Query(query): Query<CensusQuery>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let telemetry = state.supervisor.as_ref().map(|s| s.telemetry().as_ref());
    let mut records = project_census(&state.tree, telemetry);
    if let Some(slug) = query.commission.as_deref() {
        records.retain(|v| v.commission_slug.as_ref().map(|s| s.as_str()) == Some(slug));
    }

    (StatusCode::OK, Json(records)).into_response()
}

/// `GET /api/v1/ledger/commissions`
async fn commission_list_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let views = project_commissions(state.ledger.as_deref(), &state.tree);
    (StatusCode::OK, Json(views)).into_response()
}

/// `GET /api/v1/ledger/commissions/:slug`
async fn commission_detail_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let ledger = state
        .ledger
        .clone()
        .unwrap_or_else(|| Arc::new(Ledger::new(state.tree.as_ref().clone())));

    match ledger.show_commission(&slug) {
        Ok(detail) => {
            let has_brief = detail.brief.is_some();
            let has_indenture = !detail.indenture_entries.is_empty();
            let has_git = detail.git_head.is_some();
            let summary = CommissionSummaryView {
                commission_id: detail.id,
                slug: detail.slug,
                state: detail.state.kind_name().to_string(),
                roster: detail.roster.iter().map(|m| m.uid).collect(),
                deliverables_count: 0,
                brief: has_brief,
                indenture: has_indenture,
                git: if has_git {
                    "bare".to_string()
                } else {
                    "-".to_string()
                },
            };

            let indenture_tasks = detail
                .indenture_entries
                .into_iter()
                .map(|t| ApiIndentureTask {
                    uid: t.uid,
                    task: format!("{:?}", t.status),
                    done: t.status == IndentureEntryStatus::Done,
                    identity: t.identity,
                    status: t.status.as_str().to_string(),
                })
                .collect();

            let resp = CommissionDetailView {
                summary,
                brief: detail.brief,
                git_head: detail.git_head,
                main_oid: detail.main_oid,
                indenture_tasks,
                deliverables: Vec::new(),
                convergence: None,
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found(format!(
                "commission not found: {e}"
            ))),
        )
            .into_response(),
    }
}

/// `POST /api/v1/commissions/new`
async fn commission_new_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    Json(payload): Json<CommissionNewRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let ledger = state
        .ledger
        .clone()
        .unwrap_or_else(|| Arc::new(Ledger::new(state.tree.as_ref().clone())));

    let seed_path = payload.seed_dir.as_deref().map(Path::new);
    match ledger.create_commission(payload.slug.as_str(), &payload.brief, seed_path) {
        Ok(created) => {
            let resp = CommissionNewResponse {
                commission_id: created.id,
                slug: created.slug.clone(),
                message: created.summary(),
                state: created.state.kind_name().to_string(),
                ledger_dir: format!("guild/ledger/commission{}", created.id),
                git_dir: format!("guild/ledger/commission{}/git", created.id),
                brief_file: format!("guild/ledger/commission{}/COMMISSION.md", created.id),
                seed_desc: match (created.seeded_files, created.seeded_commit) {
                    (Some(n), Some(oid)) => format!("{n} files ({oid})"),
                    _ => "none (empty bare repository)".to_string(),
                },
                mayor_wake: "commission_created".to_string(),
            };
            state.publish(
                WsTopic::Commission,
                "commission_created",
                serde_json::json!({ "commission_id": resp.commission_id, "slug": resp.slug }),
            );
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(format!(
                "failed to create commission: {e}"
            ))),
        )
            .into_response(),
    }
}

/// `POST /api/v1/commissions/:slug/close`
async fn commission_close_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let ledger = state
        .ledger
        .clone()
        .unwrap_or_else(|| Arc::new(Ledger::new(state.tree.as_ref().clone())));

    match ledger.close_commission(&slug) {
        Ok(report) => {
            let resp = CommissionCloseResponse {
                commission_id: report.id,
                slug: report.slug,
                state: report.state.kind_name().to_string(),
                parcels_stopped: report.parcels_stopped,
                escalation_filed: report
                    .convergence
                    .as_ref()
                    .map(|c| !c.overall_success())
                    .unwrap_or(false),
            };
            state.publish(
                WsTopic::Commission,
                "commission_closed",
                serde_json::json!({
                    "commission_id": resp.commission_id,
                    "slug": resp.slug,
                    "parcels_stopped": resp.parcels_stopped,
                    "escalation_filed": resp.escalation_filed,
                }),
            );
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(format!(
                "failed to close commission: {e}"
            ))),
        )
            .into_response(),
    }
}

/// Pure projection of decrees in manor.
pub(crate) fn project_decrees(tree: &HamletTree) -> Vec<DecreeSummaryView> {
    let charter = match Charter::load(&tree.charter()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let dir = tree.manor_decrees();
    let mut summaries = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(id) = DecreeId::parse(stem) {
                        let status_str = match charter.decrees.get(&id.seq()) {
                            Some(DecreeStatus::Queued) => "queued",
                            Some(DecreeStatus::Delivered) => "done",
                            Some(DecreeStatus::Failed) => "rejected",
                            None => "untracked",
                        };

                        let action_kind = fs::read_to_string(&path)
                            .ok()
                            .and_then(|content| {
                                serde_json::from_str::<serde_json::Value>(&content).ok()
                            })
                            .and_then(|val| {
                                val.get("action")
                                    .and_then(|a| a.as_str())
                                    .map(ToString::to_string)
                            })
                            .unwrap_or_else(|| "unknown".to_string());

                        summaries.push(DecreeSummaryView {
                            decree_id: id.to_string(),
                            seq: id.seq(),
                            action: action_kind,
                            status: status_str.to_string(),
                            reason: None,
                        });
                    }
                }
            }
        }
    }

    summaries.sort_by_key(|d| d.seq);
    summaries
}

/// `GET /api/v1/decrees`
async fn decrees_list_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }
    (StatusCode::OK, Json(project_decrees(&state.tree))).into_response()
}

/// `GET /api/v1/decrees/:id`
async fn decree_detail_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(id_str): AxumPath<String>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let id = match DecreeId::parse(&id_str) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(format!(
                    "invalid decree id: {e}"
                ))),
            )
                .into_response();
        }
    };

    let file_path = state.tree.manor_decree_file(&id);
    let raw_json = match fs::read_to_string(&file_path) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(format!(
                    "decree not found: {e}"
                ))),
            )
                .into_response();
        }
    };

    let ack_path = state.tree.manor_decree_ack(&id);
    let ack_verdict = fs::read_to_string(&ack_path).ok();

    let action_kind = serde_json::from_str::<serde_json::Value>(&raw_json)
        .ok()
        .and_then(|val| {
            val.get("action")
                .and_then(|a| a.as_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    let summary = DecreeSummaryView {
        decree_id: id.to_string(),
        seq: id.seq(),
        action: action_kind,
        status: if ack_path.exists() {
            "acknowledged".into()
        } else {
            "pending".into()
        },
        reason: None,
    };

    let detail = DecreeDetailView {
        summary,
        raw_json,
        ack_verdict,
        ack_reason: None,
    };

    (StatusCode::OK, Json(detail)).into_response()
}

#[derive(Debug, Deserialize)]
struct VaultQuery {
    commission: Option<String>,
}

/// Pure projection of keep vault items.
pub(crate) fn project_vault(tree: &HamletTree) -> Vec<VaultItemView> {
    let vault_root = tree.keep_vault();
    let mut items = Vec::new();

    if let Ok(entries) = fs::read_dir(&vault_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = entry.file_name().to_string_lossy().to_string();
                let comm_safe = sanitize_name(&dir_name).ok();
                if let Ok(sub_entries) = fs::read_dir(&path) {
                    for sub in sub_entries.flatten() {
                        let sub_path = sub.path();
                        if sub_path.is_file()
                            && sub_path.extension().and_then(|e| e.to_str()) != Some("sha256")
                        {
                            let size_bytes = sub.metadata().map(|m| m.len()).unwrap_or(0);
                            let sidecar_name =
                                format!("{}.sha256", sub.file_name().to_string_lossy());
                            let sidecar = path.join(sidecar_name);
                            let sha256_raw = fs::read_to_string(sidecar).unwrap_or_default();
                            let sha256 = sha256_raw
                                .split_whitespace()
                                .next()
                                .unwrap_or("")
                                .to_string();

                            items.push(VaultItemView {
                                name: sub.file_name().to_string_lossy().to_string(),
                                commission: comm_safe.clone(),
                                size_bytes,
                                sha256,
                            });
                        }
                    }
                }
            } else if path.is_file() && path.extension().and_then(|e| e.to_str()) != Some("sha256")
            {
                let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let sidecar_name = format!("{}.sha256", entry.file_name().to_string_lossy());
                let sidecar = vault_root.join(sidecar_name);
                let sha256_raw = fs::read_to_string(sidecar).unwrap_or_default();
                let sha256 = sha256_raw
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();

                items.push(VaultItemView {
                    name: entry.file_name().to_string_lossy().to_string(),
                    commission: None,
                    size_bytes,
                    sha256,
                });
            }
        }
    }

    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

/// `GET /api/v1/vault`
async fn vault_list_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    Query(query): Query<VaultQuery>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let filter_comm = query.commission.as_deref();
    let mut items = project_vault(&state.tree);
    if let Some(filter) = filter_comm {
        items.retain(|i| i.commission.as_ref().map(|c| c.as_str()) == Some(filter));
    }

    (StatusCode::OK, Json(items)).into_response()
}

/// Pure helper for exporting vault artifact deliverables.
pub(crate) fn export_vault_artifact(
    tree: &HamletTree,
    payload: &VaultExportRequest,
) -> Result<VaultExportResponse, (StatusCode, String)> {
    let safe_artifact = match sanitize_name(&payload.artifact) {
        Ok(name) => name,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid artifact name '{}': {e}", payload.artifact),
            ));
        }
    };

    let vault_root = tree.keep_vault();
    let (source, resolved_commission) = if let Some(ref comm) = payload.commission {
        let p = vault_root.join(comm.as_str()).join(safe_artifact.as_str());
        if let Ok(meta) = fs::symlink_metadata(&p) {
            if meta.file_type().is_symlink() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("symlink vault path {} refused (ADR-0007)", p.display()),
                ));
            }
        }
        (p, Some(comm.clone()))
    } else {
        let direct = vault_root.join(safe_artifact.as_str());
        if direct.is_file() {
            (direct, None)
        } else {
            let mut matches = Vec::new();
            if let Ok(entries) = fs::read_dir(&vault_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Ok(meta) = fs::symlink_metadata(&path) {
                        if meta.file_type().is_symlink() {
                            return Err((
                                StatusCode::BAD_REQUEST,
                                format!(
                                    "symlink vault directory {} refused (ADR-0007)",
                                    path.display()
                                ),
                            ));
                        }
                    }
                    if path.is_dir() {
                        let candidate = path.join(safe_artifact.as_str());
                        if let Ok(meta) = fs::symlink_metadata(&candidate) {
                            if meta.file_type().is_symlink() {
                                return Err((
                                    StatusCode::BAD_REQUEST,
                                    format!(
                                        "symlink vault artifact {} refused (ADR-0007)",
                                        candidate.display()
                                    ),
                                ));
                            }
                        }
                        if candidate.is_file() {
                            matches
                                .push((entry.file_name().to_string_lossy().to_string(), candidate));
                        }
                    }
                }
            }

            if matches.len() > 1 {
                let slugs: Vec<_> = matches.into_iter().map(|(s, _)| s).collect();
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "ambiguous artifact '{}': found in commissions {}; specify --commission <slug> to disambiguate",
                        payload.artifact,
                        slugs.join(", ")
                    ),
                ));
            } else if matches.len() == 1 {
                let (slug, cand) = matches.remove(0);
                (cand, sanitize_name(&slug).ok())
            } else {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("vault artifact '{}' not found", payload.artifact),
                ));
            }
        }
    };

    let canonical_root = vault_root
        .canonicalize()
        .unwrap_or_else(|_| vault_root.clone());
    if let Ok(canonical_source) = source.canonicalize() {
        if !canonical_source.starts_with(&canonical_root) || canonical_source == canonical_root {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("vault artifact '{}' escapes vault root", payload.artifact),
            ));
        }
    }

    if !source.exists() || !source.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("vault artifact '{}' not found", payload.artifact),
        ));
    }

    let dest = PathBuf::from(&payload.destination);
    if let Ok(meta) = fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "refusing symlink destination path {} under ADR-0007",
                    dest.display()
                ),
            ));
        }
    }
    let target = if dest.is_dir() {
        dest.join(safe_artifact.as_str())
    } else {
        dest
    };

    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "refusing symlink target path {} under ADR-0007",
                    target.display()
                ),
            ));
        }
    }

    let bytes_written = match fs::copy(&source, &target) {
        Ok(b) => b,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("export copy failed: {e}"),
            ));
        }
    };

    let sidecar_name = format!(
        "{}.sha256",
        source.file_name().unwrap_or_default().to_string_lossy()
    );
    let sidecar = source
        .parent()
        .map(|p| p.join(sidecar_name))
        .unwrap_or_default();
    let sha256_raw = fs::read_to_string(sidecar).unwrap_or_default();
    let sha256 = sha256_raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    Ok(VaultExportResponse {
        artifact: safe_artifact.into_string(),
        destination: target.display().to_string(),
        bytes_written,
        sha256,
        commission: resolved_commission,
    })
}

/// `POST /api/v1/vault/export`
async fn vault_export_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    Json(payload): Json<VaultExportRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    match export_vault_artifact(&state.tree, &payload) {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err((code, msg)) => (code, Json(ApiErrorResponse::new(code.as_str(), msg))).into_response(),
    }
}

/// `GET /api/v1/metrics`
async fn metrics_handler(State(state): State<Arc<LocalApiState>>, headers: HeaderMap) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let charter = match Charter::load(&state.tree.charter()) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(format!(
                    "failed to load charter: {e}"
                ))),
            )
                .into_response();
        }
    };

    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cost = 0u64;
    let mut agent_items = Vec::new();

    if let Some(ref sup) = state.supervisor {
        let telemetry = sup.telemetry();
        for (uid, slot) in &charter.slots {
            let key = AgentKey {
                commission: slot.commission.unwrap_or(0),
                uid: *uid,
            };
            if let Ok(totals) = telemetry.agent_totals(key) {
                total_input += totals.input_tokens;
                total_output += totals.output_tokens;
                total_cost += totals.cost_micro_usd;

                agent_items.push(AgentMetricsItem {
                    uid: *uid,
                    identity: slot.identity.clone(),
                    commission: slot.commission,
                    input_tokens: totals.input_tokens,
                    output_tokens: totals.output_tokens,
                    cost_micro_usd: totals.cost_micro_usd,
                    crashes: 0,
                    uptime_secs: 0,
                });
            }
        }
    }

    let active_sessions = charter
        .sessions
        .values()
        .filter(|s| s.status == hamlet_core::session::SessionStatus::Running)
        .count();

    let resp = MetricsSummaryView {
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cost_micro_usd: total_cost,
        active_sessions,
        agents: agent_items,
    };

    (StatusCode::OK, Json(resp)).into_response()
}

/// `GET /api/v1/mail/:uid`
async fn mail_list_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(uid): AxumPath<u32>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let safe_uid = match sanitize_name(&uid.to_string()) {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request("invalid uid")),
            )
                .into_response();
        }
    };

    let mut messages = Vec::new();
    let spool = state.tree.courier_mail_recipient(&safe_uid);
    let read_spool = state.tree.courier_mail_read(&safe_uid);

    for (dir, is_read) in [(spool, false), (read_spool, true)] {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("mail") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let mut from = "unknown".to_string();
                        let mut subject = "(no subject)".to_string();
                        for line in content.lines() {
                            if let Some(f) = line.strip_prefix("From: ") {
                                from = f.trim().to_string();
                            } else if let Some(s) = line.strip_prefix("Subject: ") {
                                subject = s.trim().to_string();
                            } else if line.is_empty() {
                                break;
                            }
                        }

                        messages.push(MailMessageSummary {
                            file_name: entry.file_name().to_string_lossy().to_string(),
                            from,
                            to: uid,
                            subject,
                            read: is_read,
                        });
                    }
                }
            }
        }
    }

    messages.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    (StatusCode::OK, Json(messages)).into_response()
}

/// `GET /api/v1/mail/:uid/:file`
async fn mail_message_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath((uid, file)): AxumPath<(u32, String)>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let safe_uid = match sanitize_name(&uid.to_string()) {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request("invalid uid")),
            )
                .into_response();
        }
    };

    let safe_file = match sanitize_name(&file) {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request("invalid mail file name")),
            )
                .into_response();
        }
    };

    let spool_file = state
        .tree
        .courier_mail_recipient(&safe_uid)
        .join(safe_file.as_str());
    let read_file = state
        .tree
        .courier_mail_read(&safe_uid)
        .join(safe_file.as_str());

    let (path, is_read) = if spool_file.exists() {
        (spool_file, false)
    } else if read_file.exists() {
        (read_file, true)
    } else {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("mail message not found")),
        )
            .into_response();
    };

    match fs::read_to_string(&path) {
        Ok(content) => {
            let mut from = "unknown".to_string();
            let mut subject = "(no subject)".to_string();
            let mut body = String::new();
            let mut in_body = false;

            for line in content.lines() {
                if in_body {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(line);
                } else if line.is_empty() {
                    in_body = true;
                } else if let Some(f) = line.strip_prefix("From: ") {
                    from = f.trim().to_string();
                } else if let Some(s) = line.strip_prefix("Subject: ") {
                    subject = s.trim().to_string();
                }
            }

            let detail = MailMessageDetail {
                summary: MailMessageSummary {
                    file_name: safe_file.to_string(),
                    from,
                    to: uid,
                    subject,
                    read: is_read,
                },
                body,
            };
            (StatusCode::OK, Json(detail)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(format!(
                "failed to read mail: {e}"
            ))),
        )
            .into_response(),
    }
}

/// `POST /api/v1/mail/send`
async fn mail_send_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    Json(payload): Json<MailSendRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let sender = payload.from.as_deref().unwrap_or("operator");
    let actor = match sanitize_name(sender) {
        Ok(name) => MailActor::Patron(name),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(format!(
                    "invalid sender name: {e}"
                ))),
            )
                .into_response();
        }
    };

    if let Some(ref router) = state.mail_router {
        match router.send(actor, payload.to, &payload.subject, &payload.body, None) {
            Ok(path) => {
                let resp = MailSendResponse {
                    delivered: true,
                    spool_file: path.as_path().display().to_string(),
                };
                state.publish(
                    WsTopic::Mail,
                    "mail_sent",
                    serde_json::json!({
                        "from": sender,
                        "to": payload.to,
                        "subject": payload.subject,
                    }),
                );
                (StatusCode::OK, Json(resp)).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(format!(
                    "failed to send mail: {e}"
                ))),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::internal_error(
                "mail router not attached to daemon",
            )),
        )
            .into_response()
    }
}

pub(crate) async fn hibernate_agent(
    stasis: &ProductionStasis,
    uid: u32,
    trigger: &str,
) -> Result<hamlet_core::stasis::PatronDoc, crate::census::StasisError> {
    stasis.hibernate(uid, trigger).await
}

/// `POST /api/v1/agents/:uid/hibernate`
async fn agent_hibernate_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(uid): AxumPath<u32>,
    Json(payload): Json<AgentHibernateRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let trigger = payload.trigger.as_deref().unwrap_or("operator:api");
    if let Some(ref stasis) = state.stasis {
        match stasis.hibernate(uid, trigger).await {
            Ok(doc) => {
                let resp = AgentHibernateResponse {
                    uid,
                    patron: doc.frontmatter.patron,
                    status: "hibernated".to_string(),
                };
                state.publish(
                    WsTopic::Census,
                    "agent_hibernated",
                    serde_json::json!({ "uid": uid, "patron": resp.patron }),
                );
                (StatusCode::OK, Json(resp)).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(format!(
                    "hibernation failed: {e}"
                ))),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::internal_error(
                "stasis engine not attached",
            )),
        )
            .into_response()
    }
}

/// `POST /api/v1/agents/wake`
async fn agent_wake_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    Json(payload): Json<AgentWakeRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    let commission_id = if let Some(ref slug) = payload.commission {
        match Charter::load(&state.tree.charter()) {
            Ok(charter) => charter
                .commissions
                .iter()
                .find(|(_, rec)| rec.slug == *slug)
                .map(|(&id, _)| id),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(format!(
                        "load charter: {e}"
                    ))),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    if let Some(ref stasis) = state.stasis {
        match stasis.awaken(&payload.patron, commission_id).await {
            Ok(agent) => {
                let resp = AgentWakeResponse {
                    allocated_uid: agent.uid,
                    identity: agent.identity,
                    commission: payload.commission,
                };
                state.publish(
                    WsTopic::Census,
                    "agent_woken",
                    serde_json::json!({
                        "uid": resp.allocated_uid,
                        "identity": resp.identity,
                        "commission": resp.commission,
                    }),
                );
                (StatusCode::OK, Json(resp)).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(format!(
                    "awakening failed: {e}"
                ))),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::internal_error(
                "stasis engine not attached",
            )),
        )
            .into_response()
    }
}

/// `POST /api/v1/agents/:uid/steer`
async fn agent_steer_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(uid): AxumPath<u32>,
    Json(payload): Json<AgentSteerRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    if let Some(ref supervisor) = state.supervisor {
        match supervisor
            .steer(
                uid,
                InterruptSource::Human,
                InterruptPriority::Normal,
                &payload.directive,
            )
            .await
        {
            Ok(correlation) => {
                let resp = AgentSteerResponse {
                    uid,
                    injected: true,
                    correlation: correlation.to_string(),
                };
                state.publish(
                    WsTopic::Census,
                    "agent_steered",
                    serde_json::json!({ "uid": uid, "correlation": resp.correlation }),
                );
                (StatusCode::OK, Json(resp)).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(format!(
                    "steer injection failed: {e}"
                ))),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::internal_error("supervisor not attached")),
        )
            .into_response()
    }
}

/// `GET /api/v1/events/ws`
async fn ws_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    // Browsers cannot set WS headers, so `?token=` is honoured (ADR-0043 cl. 5).
    if let Some(res) = authenticate(&headers, raw_query.as_deref(), &state) {
        return res;
    }

    let hub = Arc::clone(&state.ws_hub);
    ws.on_upgrade(move |socket| async move {
        hub.handle_socket(socket).await;
    })
}

// ---------------------------------------------------------------------------
// Landmark & Escalation Projections (SPEC §2, §7; ADR-0043)
// ---------------------------------------------------------------------------

/// Pure projection of unread mail counts per recipient UID.
pub(crate) fn project_mail_counts(tree: &HamletTree) -> BTreeMap<u32, usize> {
    let mut counts = BTreeMap::new();
    let is_mail = |t: &fs::FileType, name: &str| t.is_file() && name.ends_with(".mail");
    for name in landmark_names(&tree.courier_mail()) {
        if let Ok(uid) = name.parse::<u32>() {
            if let Ok(safe) = sanitize_name(&name) {
                let unread = landmark_count(&tree.courier_mail_recipient(&safe), is_mail);
                counts.insert(uid, unread);
            }
        }
    }
    counts
}

/// Sorted, non-hidden entry names of `dir` (empty when absent).
pub(crate) fn landmark_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Count entries of `dir` matching `pred` on the (non-followed) file type.
pub(crate) fn landmark_count(dir: &Path, pred: impl Fn(&fs::FileType, &str) -> bool) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    e.file_type().map(|t| pred(&t, &name)).unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Regular files and their total size anywhere under `dir`. Symlinks are
/// never followed (ADR-0007): a projection must not walk out of the tree.
pub(crate) fn landmark_files_recursive(dir: &Path) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                count += 1;
                bytes += meta.len();
            }
        }
    }
    (count, bytes)
}

pub(crate) fn is_visible_file(t: &fs::FileType, name: &str) -> bool {
    t.is_file() && !name.starts_with('.')
}

/// Escalation notice files (regular, `.json`, non-hidden), sorted.
fn escalation_notice_names(tree: &HamletTree) -> Vec<String> {
    landmark_names(&tree.manor_escalations())
        .into_iter()
        .filter(|n| n.ends_with(".json"))
        .filter(|n| {
            fs::symlink_metadata(tree.manor_escalations().join(n))
                .map(|m| m.is_file())
                .unwrap_or(false)
        })
        .collect()
}

fn load_resolution(tree: &HamletTree, notice: &str) -> Option<EscalationResolution> {
    let safe = sanitize_name(notice).ok()?;
    let text = fs::read_to_string(tree.manor_escalation_resolution(&safe)).ok()?;
    EscalationResolution::parse(&text).ok()
}

/// Pure projection of the escalation inbox.
pub(crate) fn project_escalations(tree: &HamletTree) -> Vec<EscalationView> {
    escalation_notice_names(tree)
        .into_iter()
        .map(|file| {
            let resolution = load_resolution(tree, &file);
            let parsed = sanitize_name(&file).ok().and_then(|safe| {
                fs::read_to_string(tree.manor_escalation_notice(&safe))
                    .ok()
                    .map(|text| EscalationNotice::parse(&text))
            });
            let (decree_id, summary, evidence_refs, recorded_unix_ms) = match parsed {
                Some(Ok(n)) => (
                    n.decree_id.render(),
                    n.summary,
                    n.evidence_refs.iter().map(ToString::to_string).collect(),
                    n.recorded_unix_ms,
                ),
                // Honest about a notice the daemon cannot read: shown, not hidden.
                Some(Err(e)) => (
                    "?".into(),
                    format!("(unreadable notice: {e})"),
                    Vec::new(),
                    0,
                ),
                None => ("?".into(), "(unreadable notice)".into(), Vec::new(), 0),
            };
            EscalationView {
                id: file,
                decree_id,
                summary,
                evidence_refs,
                recorded_unix_ms,
                resolved: resolution.is_some(),
                resolution: resolution.map(|r| r.resolution),
            }
        })
        .collect()
}

/// Pure projection of every SPEC §2 landmark through tree accessors.
pub(crate) fn project_landmarks(tree: &HamletTree) -> LandmarksView {
    let charter = Charter::load(&tree.charter()).ok();

    let guild = GuildLandmark {
        register: landmark_names(&tree.guild_register()),
        blueprints: landmark_names(&tree.guild_blueprints()),
        ledger_commissions: landmark_count(&tree.guild_ledger(), |t, n| {
            t.is_dir() && n.starts_with("commission")
        }),
    };

    let tavern = TavernLandmark {
        gossip: landmark_names(&tree.tavern_gossip()),
        patrons: landmark_names(&tree.tavern_patrons()),
    };

    let mut spools: Vec<MailSpoolView> = landmark_names(&tree.courier_mail())
        .into_iter()
        .filter_map(|n| {
            let uid: u32 = n.parse().ok()?;
            let safe = sanitize_name(&n).ok()?;
            let is_mail = |t: &fs::FileType, name: &str| t.is_file() && name.ends_with(".mail");
            Some(MailSpoolView {
                uid,
                unread: landmark_count(&tree.courier_mail_recipient(&safe), is_mail),
                read: landmark_count(&tree.courier_mail_read(&safe), is_mail),
            })
        })
        .collect();
    spools.sort_by_key(|s| s.uid);
    let courier = CourierLandmark {
        spools,
        freight_sandboxes: landmark_count(&tree.courier_freight(), |t, n| {
            t.is_dir() && !n.starts_with('.')
        }),
        dispatch_outbox: landmark_count(&tree.courier_dispatch_outbox(), is_visible_file),
        dispatch_inbox: landmark_count(&tree.courier_dispatch_inbox(), is_visible_file),
        dispatch_dead: landmark_count(&tree.courier_dispatch_dead(), is_visible_file),
    };

    let (pkgcache_entries, pkgcache_bytes) = landmark_files_recursive(&tree.market_pkgcache());
    let market = MarketLandmark {
        skills: landmark_names(&tree.market_skills()),
        pkgcache_entries,
        pkgcache_bytes,
    };

    let mut census_parcels: Vec<u32> = landmark_names(&tree.keep_census())
        .into_iter()
        .filter_map(|n| n.strip_prefix("parcel").and_then(|u| u.parse().ok()))
        .collect();
    census_parcels.sort_unstable();
    let keep = KeepLandmark {
        vault_items: landmark_files_recursive(&tree.keep_vault()).0,
        archives: landmark_names(&tree.keep_archives()),
        census_parcels,
    };

    let mayor_record = charter
        .as_ref()
        .and_then(|c| c.sessions.get(&hamlet_core::slots::MAYOR_UID));
    let escalations = project_escalations(tree);
    let manor = ManorLandmark {
        mayor_md: tree.manor_mayor_md().is_file(),
        mayor_session: mayor_record.map(|r| r.status.to_string()),
        mayor_incarnation: mayor_record.map(|r| r.id.incarnation),
        decrees_rejected: landmark_count(&tree.manor_decrees_rejected(), |t, _| t.is_file()),
        escalations_open: escalations.iter().filter(|e| !e.resolved).count(),
        escalations_resolved: escalations.iter().filter(|e| e.resolved).count(),
    };

    LandmarksView {
        guild,
        tavern,
        courier,
        market,
        keep,
        manor,
    }
}

/// `GET /api/v1/landmarks`
async fn landmarks_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }
    (StatusCode::OK, Json(project_landmarks(&state.tree))).into_response()
}

/// `GET /api/v1/escalations`
async fn escalations_list_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }
    (StatusCode::OK, Json(project_escalations(&state.tree))).into_response()
}

/// Pure helper for resolving manor escalations.
pub(crate) fn resolve_escalation(
    tree: &HamletTree,
    mail_router: Option<&MailRouter>,
    id: &str,
    resolution: &str,
) -> Result<EscalationResolveResponse, (StatusCode, String)> {
    let safe = sanitize_name(id).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid escalation id: {e}"),
        )
    })?;
    let res_trimmed = resolution.trim();
    if res_trimmed.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "resolution must not be empty".to_string(),
        ));
    }
    let notice_ok = fs::symlink_metadata(tree.manor_escalation_notice(&safe))
        .map(|m| m.is_file())
        .unwrap_or(false);
    if !notice_ok {
        return Err((
            StatusCode::NOT_FOUND,
            format!("escalation {safe} not found"),
        ));
    }
    let record_path = tree.manor_escalation_resolution(&safe);
    if record_path.exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("escalation {safe} already resolved"),
        ));
    }

    let record = EscalationResolution {
        resolution_version: ESCALATION_RESOLUTION_VERSION,
        id: safe.to_string(),
        resolution: res_trimmed.to_string(),
        recorded_unix_ms: unix_now_ms(),
    };
    let _ = fs::create_dir_all(tree.manor_escalations_resolved());
    write_atomic_file(&record_path, record.render().as_bytes(), Some(0o644)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to record resolution: {e}"),
        )
    })?;

    let mayor_notified = match (mail_router, sanitize_name("operator")) {
        (Some(router), Ok(op)) => router
            .send(
                MailActor::Patron(op),
                hamlet_core::slots::MAYOR_UID,
                &format!("escalation {safe} resolved"),
                res_trimmed,
                None,
            )
            .is_ok(),
        _ => false,
    };

    Ok(EscalationResolveResponse {
        id: safe.to_string(),
        resolved: true,
        mayor_notified,
    })
}

/// `POST /api/v1/escalations/:id/resolve`
async fn escalation_resolve_handler(
    State(state): State<Arc<LocalApiState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(payload): Json<EscalationResolveRequest>,
) -> Response {
    if let Some(res) = authenticate(&headers, None, &state) {
        return res;
    }

    match resolve_escalation(
        &state.tree,
        state.mail_router.as_deref(),
        &id,
        &payload.resolution,
    ) {
        Ok(resp) => {
            state.publish(
                WsTopic::Decree,
                "escalation_resolved",
                serde_json::json!({ "id": resp.id, "mayor_notified": resp.mayor_notified }),
            );
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err((code, msg)) => (code, Json(ApiErrorResponse::new(code.as_str(), msg))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Router Assembly
// ---------------------------------------------------------------------------

/// Builds the local API axum router.
pub fn local_api_router(state: Arc<LocalApiState>) -> Router {
    let asset_routes = crate::web::asset_router(Arc::clone(&state.asset_server));

    let api_routes = Router::new()
        .route(ENDPOINT_STATUS, get(status_handler))
        .route(ENDPOINT_STOP, post(stop_handler))
        .route(ENDPOINT_CENSUS, get(census_handler))
        .route(ENDPOINT_COMMISSIONS, get(commission_list_handler))
        .route(ENDPOINT_COMMISSION_DETAIL, get(commission_detail_handler))
        .route(ENDPOINT_COMMISSION_NEW, post(commission_new_handler))
        .route(ENDPOINT_COMMISSION_CLOSE, post(commission_close_handler))
        .route(ENDPOINT_DECREES, get(decrees_list_handler))
        .route(ENDPOINT_DECREE_DETAIL, get(decree_detail_handler))
        .route(ENDPOINT_VAULT, get(vault_list_handler))
        .route(ENDPOINT_VAULT_EXPORT, post(vault_export_handler))
        .route(ENDPOINT_METRICS, get(metrics_handler))
        .route(ENDPOINT_MAIL, get(mail_list_handler))
        .route(ENDPOINT_MAIL_MESSAGE, get(mail_message_handler))
        .route(ENDPOINT_MAIL_SEND, post(mail_send_handler))
        .route(ENDPOINT_AGENT_HIBERNATE, post(agent_hibernate_handler))
        .route(ENDPOINT_AGENT_WAKE, post(agent_wake_handler))
        .route(ENDPOINT_AGENT_STEER, post(agent_steer_handler))
        .route(ENDPOINT_WS_EVENTS, get(ws_handler))
        .route(ENDPOINT_LANDMARKS, get(landmarks_handler))
        .route(ENDPOINT_ESCALATIONS, get(escalations_list_handler))
        .route(
            ENDPOINT_ESCALATION_RESOLVE,
            post(escalation_resolve_handler),
        )
        .with_state(state);

    api_routes.merge(asset_routes)
}

// ---------------------------------------------------------------------------
// File Lifecycle Helpers
// ---------------------------------------------------------------------------

pub(crate) fn write_atomic_file(dest: &Path, content: &[u8], mode: Option<u32>) -> io::Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    let pid = std::process::id();
    let file_name = dest.file_name().and_then(|f| f.to_str()).unwrap_or("file");
    let tmp_path = parent.join(format!(".{file_name}.tmp.{pid}"));

    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    if let Some(m) = mode {
        opts.mode(m);
    }
    let mut file = opts.open(&tmp_path)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp_path, dest)?;
    if let Ok(parent_dir) = fs::File::open(parent) {
        let _ = parent_dir.sync_all();
    }
    Ok(())
}

/// Writes `daemon.token`, `daemon.pid`, and `daemon.endpoint` files atomically
/// (temp → fsync → rename) per CONVENTIONS §3.
///
/// # Errors
///
/// I/O error on file creation or permissions error.
pub fn write_daemon_files(
    tree: &HamletTree,
    endpoint: &DaemonEndpoint,
    token: &LocalToken,
) -> io::Result<()> {
    write_atomic_file(&tree.daemon_token(), token.as_str().as_bytes(), Some(0o600))?;
    write_atomic_file(
        &tree.daemon_pid(),
        std::process::id().to_string().as_bytes(),
        None,
    )?;
    let json = serde_json::to_string_pretty(endpoint).map_err(io::Error::other)?;
    write_atomic_file(&tree.daemon_endpoint(), json.as_bytes(), None)?;
    Ok(())
}

/// Generates a cryptographic or random 32-byte hex local token.
pub fn generate_local_token() -> LocalToken {
    let mut bytes = [0u8; 32];
    let mut filled = false;
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut bytes).is_ok() {
            filled = true;
        }
    }
    if !filled {
        let now = unix_now_ms();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((now >> ((i % 8) * 8)) ^ ((std::process::id() as u64) << (i % 4))) as u8;
        }
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    LocalToken::new(hex)
}

/// Removes `daemon.token`, `daemon.pid`, and `daemon.endpoint` files on shutdown.
pub fn cleanup_daemon_files(tree: &HamletTree) -> io::Result<()> {
    let _ = fs::remove_file(tree.daemon_token());
    let _ = fs::remove_file(tree.daemon_pid());
    let _ = fs::remove_file(tree.daemon_endpoint());
    Ok(())
}

/// Runs the local daemon server over a village tree.
pub async fn serve_daemon(
    tree: Arc<HamletTree>,
    bind_port: Option<u16>,
    mut external_shutdown_rx: Option<watch::Receiver<bool>>,
) -> io::Result<()> {
    if !tree.root().exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("village root {} does not exist", tree.root().display()),
        ));
    }

    // Check if daemon is already running
    let pid_path = tree.daemon_pid();
    if pid_path.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if Path::new(&format!("/proc/{pid}")).exists() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("daemon is already running with PID {pid}"),
                    ));
                }
            }
        }
        let _ = cleanup_daemon_files(&tree);
    }

    let token = generate_local_token();
    let (shutdown_tx, mut internal_shutdown_rx) = watch::channel(false);

    let requested_port = bind_port.unwrap_or(0);
    let listener = match TcpListener::bind(format!("127.0.0.1:{requested_port}")).await {
        Ok(l) => l,
        Err(_) if requested_port != 0 => TcpListener::bind("127.0.0.1:0").await?,
        Err(e) => return Err(e),
    };

    let local_addr = listener.local_addr()?;
    let port = local_addr.port();
    let pid = std::process::id();
    let started_at = unix_now_ms();

    let endpoint = DaemonEndpoint::new("127.0.0.1", port, pid, started_at);
    write_daemon_files(&tree, &endpoint, &token)?;

    let telemetry_path = tree.root().join(".hamlet-data");
    let telemetry =
        Arc::new(Telemetry::open(telemetry_path).map_err(|e| io::Error::other(e.to_string()))?);

    let config = if let Ok(s) = fs::read_to_string(tree.config()) {
        hamlet_core::config::Config::parse(&s).unwrap_or_default()
    } else {
        hamlet_core::config::Config::default()
    };
    let fakeagent_path =
        std::env::var("HAMLET_FAKEAGENT_BIN").unwrap_or_else(|_| "/bin/true".to_string());
    let spawner: Arc<dyn crate::supervise::SessionSpawner> =
        Arc::new(crate::supervise::HostProcessSpawner::new(&fakeagent_path));
    let supervisor = Arc::new(Supervisor::new(
        (*tree).clone(),
        config.clone(),
        spawner,
        Arc::clone(&telemetry),
    ));
    let mail_router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
    let ledger = Arc::new(Ledger::new((*tree).clone()));
    let runtime: Arc<dyn crate::podman::Runtime> = Arc::new(crate::podman::FakeRuntime::default());
    let stasis =
        Arc::new(ProductionStasis::new((*tree).clone(), runtime).with_telemetry(telemetry.clone()));

    let state = Arc::new(
        LocalApiState::new(tree.clone(), token.clone(), shutdown_tx.clone())
            .with_supervisor(supervisor)
            .with_mail_router(mail_router)
            .with_ledger(ledger)
            .with_stasis(stasis),
    );

    let router = local_api_router(state);

    let tree_cleanup = tree.clone();
    let server_fut = axum::serve(listener, router).with_graceful_shutdown(async move {
        tokio::select! {
            _ = internal_shutdown_rx.changed() => {},
            _ = async {
                if let Some(ref mut ext) = external_shutdown_rx {
                    let _ = ext.changed().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    });

    let res = server_fut.await.map_err(io::Error::other);
    let _ = cleanup_daemon_files(&tree_cleanup);
    res
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
