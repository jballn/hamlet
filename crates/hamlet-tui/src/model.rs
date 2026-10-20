//! Data models, room enum, state structures, and UI state for `hamlet-tui`.

use hamlet_core::api::*;
use serde::{Deserialize, Serialize};

/// The six SPEC §2 village landmark rooms (ADR-0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Room {
    Guild = 0,
    Tavern = 1,
    Courier = 2,
    Market = 3,
    Keep = 4,
    Manor = 5,
}

impl Room {
    pub const ALL: [Room; 6] = [
        Room::Guild,
        Room::Tavern,
        Room::Courier,
        Room::Market,
        Room::Keep,
        Room::Manor,
    ];

    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    #[must_use]
    pub fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Room::Guild),
            1 => Some(Room::Tavern),
            2 => Some(Room::Courier),
            3 => Some(Room::Market),
            4 => Some(Room::Keep),
            5 => Some(Room::Manor),
            _ => None,
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "1" | "guild" => Some(Room::Guild),
            "2" | "tavern" => Some(Room::Tavern),
            "3" | "courier" => Some(Room::Courier),
            "4" | "market" => Some(Room::Market),
            "5" | "keep" => Some(Room::Keep),
            "6" | "manor" => Some(Room::Manor),
            _ => None,
        }
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Room::Guild => "Guild Hall",
            Room::Tavern => "Boar's Tusk Tavern",
            Room::Courier => "Courier Post",
            Room::Market => "The Bazaar",
            Room::Keep => "Stone Keep",
            Room::Manor => "Mayor's Manor",
        }
    }

    #[must_use]
    pub fn short_name(self) -> &'static str {
        match self {
            Room::Guild => "Guild",
            Room::Tavern => "Tavern",
            Room::Courier => "Courier",
            Room::Market => "Market",
            Room::Keep => "Keep",
            Room::Manor => "Manor",
        }
    }

    #[must_use]
    pub fn tree_path(self) -> &'static str {
        match self {
            Room::Guild => "guild/",
            Room::Tavern => "tavern/",
            Room::Courier => "courier/",
            Room::Market => "market/",
            Room::Keep => "keep/",
            Room::Manor => "keep/census/manor/",
        }
    }

    #[must_use]
    pub fn mud_description(self) -> &'static str {
        match self {
            Room::Guild => {
                "The Grand Hall of the Guild of Crafters. Contracts are nailed to the heavy oak \
                 board, indenture scrolls sealed with the Mayor's wax, and blueprints line the \
                 architectural shelves."
            }
            Room::Tavern => {
                "The Boar's Tusk Tavern. The hearth crackles warmly. In the corner booths, \
                 hibernated patrons rest in quiet stasis until summoned for a commission. \
                 Rumors and user context circulate freely in the gossip feed."
            }
            Room::Courier => {
                "The Courier Post. Carrier pigeons flutter in the rafters. Cubbies hold mail spools \
                 for each resident UID, freight crates await sandboxed transit, and dispatch riders \
                 queue incoming and outgoing packets."
            }
            Room::Market => {
                "The Open Bazaar. Stalls trade in modular skills and environment blueprints. The \
                 host package cache hums efficiently, serving dependencies to isolated parcels."
            }
            Room::Keep => {
                "The Stone Keep. Guarded vault chambers hold ratified deliverables. In the census \
                 wing, parcel records track active agent lives, execution telemetry, and resource \
                 expenditure."
            }
            Room::Manor => {
                "The Mayor's Manor. In the inner study sits Trunk Agent UID 1000. Decrees are \
                 dispatched from the ledger desk, and urgent escalation notices await operator \
                 intervention."
            }
        }
    }

    #[must_use]
    pub fn next(self) -> Self {
        Self::from_index((self.index() + 1) % Self::ALL.len()).unwrap_or(Room::Guild)
    }

    #[must_use]
    pub fn prev(self) -> Self {
        let len = Self::ALL.len();
        Self::from_index((self.index() + len - 1) % len).unwrap_or(Room::Manor)
    }

    /// Determines if a WebSocket topic is relevant to this room.
    #[must_use]
    pub fn matches_topic(self, topic: WsTopic) -> bool {
        match self {
            Room::Guild => matches!(topic, WsTopic::Commission | WsTopic::Git | WsTopic::Deed),
            Room::Tavern => matches!(topic, WsTopic::Census | WsTopic::Telemetry),
            Room::Courier => matches!(topic, WsTopic::Mail | WsTopic::Freight),
            Room::Market => matches!(topic, WsTopic::Freight | WsTopic::System),
            Room::Keep => matches!(
                topic,
                WsTopic::Census | WsTopic::Telemetry | WsTopic::Freight
            ),
            Room::Manor => matches!(topic, WsTopic::Decree | WsTopic::System),
        }
    }
}

/// Daemon connectivity status.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionStatus {
    Connected,
    Reconnecting {
        attempt: usize,
        next_retry_ms: u64,
        error: String,
    },
    Disconnected(String),
}

/// A recorded WebSocket event in the ticker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickerEvent {
    pub seq: u64,
    pub timestamp_unix_ms: u64,
    pub topic: WsTopic,
    pub event_kind: String,
    pub summary: String,
    pub payload: serde_json::Value,
}

impl From<WsEvent> for TickerEvent {
    fn from(ev: WsEvent) -> Self {
        let summary = match ev.event_kind.as_str() {
            "mail_sent" => {
                let to = ev.payload.get("to").and_then(|v| v.as_u64()).unwrap_or(0);
                let subj = ev
                    .payload
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!("Mail delivered to UID {to}: {subj}")
            }
            "mail_received" => "New mail arrived in courier spool".into(),
            "agent_steered" => {
                let uid = ev.payload.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Directive injected into agent UID {uid}")
            }
            "agent_hibernated" => {
                let uid = ev.payload.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
                let patron = ev
                    .payload
                    .get("patron")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                format!("Agent UID {uid} hibernated as patron '{patron}'")
            }
            "agent_woken" => {
                let uid = ev.payload.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
                let identity = ev
                    .payload
                    .get("identity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                format!("Patron awakened as agent UID {uid} ('{identity}')")
            }
            "commission_created" => {
                let slug = ev
                    .payload
                    .get("slug")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!("New commission registered: {slug}")
            }
            "commission_closed" => {
                let slug = ev
                    .payload
                    .get("slug")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!("Commission closed: {slug}")
            }
            "decree_executed" => "Decree executed by daemon".into(),
            "escalation_resolved" => {
                let id = ev.payload.get("id").and_then(|v| v.as_str()).unwrap_or("");
                format!("Escalation notice resolved: {id}")
            }
            _ => format!("{}: {}", ev.topic, ev.event_kind),
        };

        Self {
            seq: ev.seq,
            timestamp_unix_ms: ev.timestamp_unix_ms,
            topic: ev.topic,
            event_kind: ev.event_kind,
            summary,
            payload: ev.payload,
        }
    }
}

/// Buffer and filters for the live event ticker.
#[derive(Debug, Clone, Default)]
pub struct TickerState {
    pub events: Vec<TickerEvent>,
    pub max_events: usize,
    pub filter_by_room: bool,
}

impl TickerState {
    #[must_use]
    pub fn new(max_events: usize) -> Self {
        Self {
            events: Vec::new(),
            max_events,
            filter_by_room: false,
        }
    }

    pub fn push(&mut self, event: TickerEvent) {
        if self.events.len() >= self.max_events {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    #[must_use]
    pub fn visible_events(&self, current_room: Room) -> Vec<&TickerEvent> {
        if self.filter_by_room {
            self.events
                .iter()
                .filter(|e| current_room.matches_topic(e.topic))
                .collect()
        } else {
            self.events.iter().collect()
        }
    }
}

/// Active focus field in Steer dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerFocus {
    Uid,
    Directive,
    Submit,
}

/// Active focus field in Mail Compose dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailFocus {
    To,
    Subject,
    Body,
    Submit,
}

/// Active modal or intervention surface.
#[derive(Debug, Clone, PartialEq)]
pub enum ModalState {
    None,
    Help,
    Steer {
        uid_input: String,
        directive_input: String,
        focus: SteerFocus,
        submitting: bool,
        error_msg: Option<String>,
    },
    DecreeReview {
        decree_id: String,
        detail: Option<DecreeDetailView>,
        scroll: usize,
    },
    EscalationInbox {
        selected: usize,
        notices: Vec<EscalationView>,
        resolving: bool,
        resolution_input: String,
        submitting: bool,
        status_msg: Option<String>,
    },
    MailCompose {
        to_input: String,
        subject_input: String,
        body_input: String,
        focus: MailFocus,
        submitting: bool,
        status_msg: Option<String>,
    },
    Detail {
        title: String,
        content: String,
        scroll: usize,
    },
}

/// Selection tracking per room.
#[derive(Debug, Clone, Default)]
pub struct RoomSelection {
    pub list_index: usize,
    pub sub_pane: usize,
}

/// Village state snapshot projected from daemon API.
#[derive(Debug, Clone, Default)]
pub struct VillageData {
    pub status: Option<DaemonStatusResponse>,
    pub landmarks: LandmarksView,
    pub census: Vec<CensusRecordView>,
    pub commissions: Vec<CommissionSummaryView>,
    pub commission_details: std::collections::BTreeMap<String, CommissionDetailView>,
    pub decrees: Vec<DecreeSummaryView>,
    pub decree_details: std::collections::BTreeMap<String, DecreeDetailView>,
    pub escalations: Vec<EscalationView>,
    pub mail: Vec<MailMessageSummary>,
    pub vault: Vec<VaultItemView>,
    pub metrics: Option<MetricsSummaryView>,
}

/// Overall application view-model.
#[derive(Debug, Clone)]
pub struct AppState {
    pub current_room: Room,
    pub connection: ConnectionStatus,
    pub data: VillageData,
    pub ticker: TickerState,
    pub modal: ModalState,
    pub room_sel: [RoomSelection; 6],
    pub demo_mode: bool,
    pub should_quit: bool,
    pub status_message: Option<(String, bool)>, // (message, is_error)
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            current_room: Room::Guild,
            connection: ConnectionStatus::Connected,
            data: VillageData::default(),
            ticker: TickerState::new(100),
            modal: ModalState::None,
            room_sel: Default::default(),
            demo_mode: false,
            should_quit: false,
            status_message: None,
        }
    }
}

impl AppState {
    #[must_use]
    pub fn new(initial_room: Option<Room>, demo_mode: bool) -> Self {
        let mut state = Self::default();
        if let Some(r) = initial_room {
            state.current_room = r;
        }
        state.demo_mode = demo_mode;
        state
    }

    #[must_use]
    pub fn current_sel(&self) -> &RoomSelection {
        &self.room_sel[self.current_room.index()]
    }

    pub fn current_sel_mut(&mut self) -> &mut RoomSelection {
        let idx = self.current_room.index();
        &mut self.room_sel[idx]
    }

    pub fn set_room(&mut self, room: Room) {
        self.current_room = room;
    }

    pub fn next_room(&mut self) {
        self.current_room = self.current_room.next();
    }

    pub fn prev_room(&mut self) {
        self.current_room = self.current_room.prev();
    }

    pub fn open_modal(&mut self, modal: ModalState) {
        self.modal = modal;
    }

    pub fn close_modal(&mut self) {
        self.modal = ModalState::None;
    }

    pub fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status_message = Some((msg.into(), is_error));
    }

    pub fn clear_status(&mut self) {
        self.status_message = None;
    }
}
