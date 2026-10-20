//! Local API client for `hamlet-cli` (SPEC §12.1; ADR-0041, ADR-0042).
//!
//! The CLI holds **no orchestration state** and never accesses `.hamlet/`
//! directly except for bootstrap (`init`) and discovering the daemon endpoint
//! and token (`daemon.endpoint`, `daemon.token`).

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use hamlet_core::api::*;
use hamlet_core::tree::HamletTree;
use reqwest::{header, Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Errors returned by the local API client.
#[derive(Debug)]
pub enum ClientError {
    /// Command usage / parse error (maps to exit code 2).
    Usage(String),
    /// Daemon is not running, discovery files are missing, or connection failed (maps to exit code 3).
    DaemonUnreachable(String),
    /// Typed API error returned by daemon (maps to exit code 1).
    Api(ApiErrorResponse),
    /// Operational / domain error (maps to exit code 1).
    Operational(String),
}

impl ClientError {
    /// Return the normative exit code for this error per ADR-0042.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::DaemonUnreachable(_) => 3,
            Self::Api(_) | Self::Operational(_) => 1,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg) => write!(f, "{msg}"),
            Self::DaemonUnreachable(msg) => write!(f, "{msg}"),
            Self::Api(api_err) => write!(f, "{}: {}", api_err.error, api_err.message),
            Self::Operational(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Thin HTTP client over the daemon's localhost API.
#[derive(Clone)]
pub struct HamletClient {
    tree: HamletTree,
    endpoint: DaemonEndpoint,
    token: LocalToken,
    client: Client,
}

impl HamletClient {
    /// Discover daemon endpoint and token for a village directory.
    pub fn discover(dir_flag: Option<&Path>) -> Result<Self, ClientError> {
        let tree = resolve_village_tree(dir_flag)?;

        let endpoint_path = tree.daemon_endpoint();
        let token_path = tree.daemon_token();

        if !endpoint_path.exists() || !token_path.exists() {
            return Err(ClientError::DaemonUnreachable(format!(
                "daemon is not running: discovery file {} not found",
                endpoint_path.display()
            )));
        }

        let endpoint_content = fs::read_to_string(&endpoint_path).map_err(|e| {
            ClientError::DaemonUnreachable(format!(
                "failed to read daemon endpoint file {}: {e}",
                endpoint_path.display()
            ))
        })?;

        let endpoint: DaemonEndpoint = serde_json::from_str(&endpoint_content).map_err(|e| {
            ClientError::DaemonUnreachable(format!(
                "malformed daemon endpoint file {}: {e}",
                endpoint_path.display()
            ))
        })?;

        let token_content = fs::read_to_string(&token_path).map_err(|e| {
            ClientError::DaemonUnreachable(format!(
                "failed to read daemon token file {}: {e}",
                token_path.display()
            ))
        })?;

        let token = LocalToken::new(token_content.trim());
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| {
                ClientError::Operational(format!("failed to initialize HTTP client: {e}"))
            })?;

        Ok(Self {
            tree,
            endpoint,
            token,
            client,
        })
    }

    /// Access the resolved village tree layout.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Access the discovered daemon endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &DaemonEndpoint {
        &self.endpoint
    }

    /// Access the daemon authentication token.
    #[must_use]
    pub fn token(&self) -> &LocalToken {
        &self.token
    }

    /// Perform a GET request to the local API.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.endpoint.base_url());
        let res = self
            .client
            .get(&url)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token.as_str()),
            )
            .header(LOCAL_TOKEN_HEADER, self.token.as_str())
            .send()
            .await
            .map_err(|e| ClientError::DaemonUnreachable(format!("connection failed: {e}")))?;

        Self::handle_response(res).await
    }

    /// Perform a POST request with JSON payload to the local API.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.endpoint.base_url());
        let res = self
            .client
            .post(&url)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token.as_str()),
            )
            .header(LOCAL_TOKEN_HEADER, self.token.as_str())
            .json(body)
            .send()
            .await
            .map_err(|e| ClientError::DaemonUnreachable(format!("connection failed: {e}")))?;

        Self::handle_response(res).await
    }

    /// Perform a POST request with empty body to the local API.
    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.endpoint.base_url());
        let res = self
            .client
            .post(&url)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token.as_str()),
            )
            .header(LOCAL_TOKEN_HEADER, self.token.as_str())
            .send()
            .await
            .map_err(|e| ClientError::DaemonUnreachable(format!("connection failed: {e}")))?;

        Self::handle_response(res).await
    }

    async fn handle_response<T: DeserializeOwned>(
        res: reqwest::Response,
    ) -> Result<T, ClientError> {
        let status = res.status();
        if status.is_success() {
            let data = res.json::<T>().await.map_err(|e| {
                ClientError::Operational(format!("failed to parse response body: {e}"))
            })?;
            return Ok(data);
        }

        if status == StatusCode::UNAUTHORIZED {
            return Err(ClientError::Api(ApiErrorResponse::unauthorized(
                "daemon local token rejected",
            )));
        }

        // Try reading typed ApiErrorResponse
        if let Ok(err) = res.json::<ApiErrorResponse>().await {
            return Err(ClientError::Api(err));
        }

        Err(ClientError::Operational(format!(
            "server returned unexpected status {status}"
        )))
    }
}

/// Resolves the village `.hamlet` tree from explicit flag or working directory traversal.
pub fn resolve_village_tree(dir_flag: Option<&Path>) -> Result<HamletTree, ClientError> {
    if let Some(dir) = dir_flag {
        let root = if dir.ends_with(".hamlet") {
            dir.to_path_buf()
        } else {
            dir.join(".hamlet")
        };
        return Ok(HamletTree::new(root));
    }

    // Traverse upwards from current_dir
    let cwd = std::env::current_dir().map_err(|e| {
        ClientError::Operational(format!("failed to determine current directory: {e}"))
    })?;

    let mut current = Some(cwd.as_path());
    while let Some(dir) = current {
        let candidate = dir.join(".hamlet");
        if candidate.is_dir() {
            return Ok(HamletTree::new(candidate));
        }
        current = dir.parent();
    }

    // Default to ./.hamlet in current directory
    Ok(HamletTree::new(cwd.join(".hamlet")))
}
