//! Pixel-UI work management panels and projection models (SPEC §12.3; ADR-0044).
//!
//! # Architecture & UI Model (DOM-Hybrid Pixel UI)
//!
//! Management panels are implemented via a pure-Rust projection model in the WASM view-model,
//! rendered into an accessible 8-bit DOM overlay by the thin JS glue layer.
//! Every mutation follows the command frame contract:
//! 1. Client prepares and issues a typed `WsClientFrame::Command(WsCommandFrame)`.
//! 2. View-model tracks an optimistic mutation state.
//! 3. Server responds with a correlated `WsServerFrame::CommandAck(CommandAckFrame)`.
//! 4. If rejected (`ack.success == false`), optimistic UI rolls back immediately.
//! 5. Projections derive exclusively from state-delta frames — no panel invents state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use hamlet_core::api::{
    ApiCensusCounters, CommissionSummaryView, DecreeSummaryView, EscalationView, VaultItemView,
};
use hamlet_core::sanitize::SafeName;

/// Type of work management panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKind {
    Agent,
    Guild,
    Vault,
    DecreeDesk,
    Courier,
    Tavern,
}

/// Optimistic UI command mutation lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    Pending,
    Confirmed,
    RolledBack,
}

/// Record of an in-flight optimistic command mutation awaiting server ACK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptimisticMutation {
    pub correlation_id: String,
    pub description: String,
    pub target: String,
    pub timestamp_unix_ms: u64,
    pub status: MutationStatus,
    pub error_message: Option<String>,
}

/// Agent management panel projection (clicked agent sprite).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPanelView {
    pub uid: u32,
    pub name: String,
    pub state: String,
    pub stasis: bool,
    pub commission_id: Option<u64>,
    pub commission_slug: Option<String>,
    pub session_count: u32,
    pub counters: ApiCensusCounters,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_micro_usd: u64,
    pub latency_ms: u64,
    pub memory_bytes: u64,
    pub cpu_percent_x10: u32,
    pub title_digest: String,
    pub lineage: Vec<String>,
    pub deed_summary: String,
    pub proprietor_summary: String,
    pub can_steer: bool,
    pub can_hibernate: bool,
    pub can_wake: bool,
}

/// Indenture subtask entry for an agent on the guild board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndentureEntryView {
    pub uid: u32,
    pub agent_name: String,
    pub goal: String,
    pub status: String,
}

/// Guild board panel projection (Guild Hall landmark).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildBoardView {
    pub commissions: Vec<CommissionSummaryView>,
    pub active_count: usize,
    pub draft_count: usize,
    pub closed_count: usize,
    pub registered_agents: Vec<String>,
    pub blueprints: Vec<String>,
    pub indenture_entries: Vec<IndentureEntryView>,
    pub can_create: bool,
    pub can_close: bool,
}

/// Vault panel projection (Stone Keep & Vault landmark).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultPanelView {
    pub artifacts: Vec<VaultItemView>,
    pub total_count: usize,
    pub total_bytes: u64,
    pub can_export: bool,
}

/// Decree desk panel projection (Mayor's Manor landmark).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecreeDeskView {
    pub decrees: Vec<DecreeSummaryView>,
    pub pending_count: usize,
    pub escalations: Vec<EscalationView>,
    pub can_replay: bool,
    pub can_resolve: bool,
}

/// Courier post panel projection (Courier Post landmark).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CourierPanelView {
    pub mail_counts: BTreeMap<u32, usize>,
    pub total_unread: usize,
    pub freight_sandboxes_count: usize,
    pub dispatch_outbox_count: usize,
    pub dispatch_inbox_count: usize,
    pub can_compose: bool,
}

/// Patron record in Tavern stasis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatronView {
    pub uid: u32,
    pub identity: SafeName,
    pub last_commission: Option<String>,
    pub title_digest: String,
}

/// Tavern panel projection (Boar's Tusk Tavern & Stasis landmark).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TavernPanelView {
    pub patrons: Vec<PatronView>,
    pub stasis_count: usize,
    pub gossip_files: Vec<String>,
    pub can_awaken: bool,
}

/// Active management panel projection emitted to the UI layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ActivePanel {
    None,
    Agent(AgentPanelView),
    Guild(GuildBoardView),
    Vault(VaultPanelView),
    DecreeDesk(DecreeDeskView),
    Courier(CourierPanelView),
    Tavern(TavernPanelView),
}
