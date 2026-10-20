//! Daemon API HTTP and WebSocket client for `hamlet-tui` (ADR-0041, ADR-0043).
//!
//! Holds **zero orchestration logic** and never modifies `.hamlet/` directly.

use std::fs;
use std::path::Path;
use std::time::Duration;

use hamlet_core::api::*;
use hamlet_core::tree::HamletTree;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::model::VillageData;

/// Thin client over the daemon's localhost API.
#[derive(Clone)]
pub struct TuiClient {
    endpoint: DaemonEndpoint,
    token: LocalToken,
    http: Client,
}

impl TuiClient {
    /// Construct client with explicit endpoint and token.
    #[must_use]
    pub fn new(endpoint: DaemonEndpoint, token: LocalToken) -> Self {
        let mut headers = HeaderMap::new();
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", token.as_str())) {
            headers.insert(header::AUTHORIZATION, val);
        }
        if let Ok(val) = HeaderValue::from_str(token.as_str()) {
            headers.insert(LOCAL_TOKEN_HEADER, val);
        }

        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()
            .unwrap_or_default();

        Self {
            endpoint,
            token,
            http,
        }
    }

    /// Discover daemon endpoint and token from a village tree.
    pub fn discover(dir_flag: Option<&Path>) -> Result<Self, String> {
        let tree = resolve_village_tree(dir_flag)?;

        // Try standard tree path (e.g. .hamlet/daemon.endpoint or .hamlet/run/daemon.endpoint)
        let candidates_ep = [
            tree.daemon_endpoint(),
            tree.root().join("run").join("daemon.endpoint"),
        ];
        let endpoint_path = candidates_ep.iter().find(|p| p.exists()).ok_or_else(|| {
            format!(
                "daemon discovery file not found in {}",
                tree.root().display()
            )
        })?;

        let candidates_token = [
            tree.daemon_token(),
            tree.root().join("run").join("daemon.token"),
        ];
        let token_path = candidates_token
            .iter()
            .find(|p| p.exists())
            .ok_or_else(|| format!("daemon token file not found in {}", tree.root().display()))?;

        let ep_content = fs::read_to_string(endpoint_path)
            .map_err(|e| format!("read {}: {e}", endpoint_path.display()))?;
        let endpoint: DaemonEndpoint = serde_json::from_str(&ep_content)
            .map_err(|e| format!("parse {}: {e}", endpoint_path.display()))?;

        let token_content = fs::read_to_string(token_path)
            .map_err(|e| format!("read {}: {e}", token_path.display()))?;
        let token = LocalToken::new(token_content.trim());

        Ok(Self::new(endpoint, token))
    }

    #[must_use]
    pub fn endpoint(&self) -> &DaemonEndpoint {
        &self.endpoint
    }

    #[must_use]
    pub fn token(&self) -> &LocalToken {
        &self.token
    }

    /// Perform a GET request.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{path}", self.endpoint.base_url());
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("connection to daemon failed: {e}"))?;

        Self::handle_response(res).await
    }

    /// Perform a POST request with JSON payload.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let url = format!("{}{path}", self.endpoint.base_url());
        let res = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("connection to daemon failed: {e}"))?;

        Self::handle_response(res).await
    }

    async fn handle_response<T: DeserializeOwned>(res: reqwest::Response) -> Result<T, String> {
        let status = res.status();
        if status.is_success() {
            return res
                .json::<T>()
                .await
                .map_err(|e| format!("failed to parse response: {e}"));
        }

        if status == StatusCode::UNAUTHORIZED {
            return Err("daemon local token unauthorized".into());
        }

        if let Ok(err_envelope) = res.json::<ApiErrorResponse>().await {
            return Err(format!("{}: {}", err_envelope.error, err_envelope.message));
        }

        Err(format!("server returned status {status}"))
    }

    /// Fetch full snapshot of all village state projections.
    pub async fn refresh_village_data(&self) -> Result<VillageData, String> {
        let status: Result<DaemonStatusResponse, _> = self.get(ENDPOINT_STATUS).await;
        let landmarks: LandmarksView = self.get(ENDPOINT_LANDMARKS).await.unwrap_or_default();
        let census: Vec<CensusRecordView> = self.get(ENDPOINT_CENSUS).await.unwrap_or_default();
        let commissions: Vec<CommissionSummaryView> =
            self.get(ENDPOINT_COMMISSIONS).await.unwrap_or_default();
        let decrees: Vec<DecreeSummaryView> = self.get(ENDPOINT_DECREES).await.unwrap_or_default();
        let escalations: Vec<EscalationView> =
            self.get(ENDPOINT_ESCALATIONS).await.unwrap_or_default();
        let vault: Vec<VaultItemView> = self.get(ENDPOINT_VAULT).await.unwrap_or_default();
        let metrics: Option<MetricsSummaryView> = self.get(ENDPOINT_METRICS).await.ok();

        // Mail for Mayor (UID 1000)
        let mail: Vec<MailMessageSummary> = self
            .get(&endpoint_path(ENDPOINT_MAIL, &[("uid", "1000")]))
            .await
            .unwrap_or_default();

        Ok(VillageData {
            status: status.ok(),
            landmarks,
            census,
            commissions,
            commission_details: std::collections::BTreeMap::new(),
            decrees,
            decree_details: std::collections::BTreeMap::new(),
            escalations,
            mail,
            vault,
            metrics,
        })
    }

    /// Fetch detailed view of a commission.
    pub async fn fetch_commission_detail(
        &self,
        slug: &str,
    ) -> Result<CommissionDetailView, String> {
        self.get(&endpoint_path(
            ENDPOINT_COMMISSION_DETAIL,
            &[("slug", slug)],
        ))
        .await
    }

    /// Fetch detailed view of a decree.
    pub async fn fetch_decree_detail(&self, id: &str) -> Result<DecreeDetailView, String> {
        self.get(&endpoint_path(ENDPOINT_DECREE_DETAIL, &[("id", id)]))
            .await
    }

    /// Steer an agent session.
    pub async fn steer_agent(
        &self,
        uid: u32,
        directive: &str,
    ) -> Result<AgentSteerResponse, String> {
        let uid_str = uid.to_string();
        self.post(
            &endpoint_path(ENDPOINT_AGENT_STEER, &[("uid", &uid_str)]),
            &AgentSteerRequest {
                directive: directive.to_string(),
            },
        )
        .await
    }

    /// Send a mail message.
    pub async fn send_mail(
        &self,
        to: u32,
        from: Option<&str>,
        subject: &str,
        body: &str,
    ) -> Result<MailSendResponse, String> {
        self.post(
            ENDPOINT_MAIL_SEND,
            &MailSendRequest {
                to,
                from: from.map(ToString::to_string),
                subject: subject.to_string(),
                body: body.to_string(),
            },
        )
        .await
    }

    /// Resolve an escalation notice.
    pub async fn resolve_escalation(
        &self,
        id: &str,
        resolution: &str,
    ) -> Result<EscalationResolveResponse, String> {
        self.post(
            &endpoint_path(ENDPOINT_ESCALATION_RESOLVE, &[("id", id)]),
            &EscalationResolveRequest {
                resolution: resolution.to_string(),
            },
        )
        .await
    }

    /// Spawns a background WebSocket listener streaming events.
    pub fn spawn_ws_listener(&self, tx: mpsc::Sender<WsEvent>) {
        let ws_url = format!("{}?token={}", self.endpoint.ws_url(), self.token.as_str());

        std::thread::spawn(move || loop {
            match tokio_tungstenite::tungstenite::connect(&ws_url) {
                Ok((mut socket, _)) => loop {
                    match socket.read() {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                                if tx.blocking_send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                        Err(_) => break,
                        _ => {}
                    }
                },
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1500));
                }
            }
            std::thread::sleep(Duration::from_millis(1000));
        });
    }
}

/// Helper to resolve the `.hamlet` tree directory.
pub fn resolve_village_tree(dir_flag: Option<&Path>) -> Result<HamletTree, String> {
    if let Some(dir) = dir_flag {
        let root = if dir.ends_with(".hamlet") {
            dir.to_path_buf()
        } else {
            dir.join(".hamlet")
        };
        return Ok(HamletTree::new(root));
    }

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut current = Some(cwd.as_path());
    while let Some(dir) = current {
        let candidate = dir.join(".hamlet");
        if candidate.is_dir() {
            return Ok(HamletTree::new(candidate));
        }
        current = dir.parent();
    }

    Ok(HamletTree::new(cwd.join(".hamlet")))
}
