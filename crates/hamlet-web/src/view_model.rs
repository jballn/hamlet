//! Pure village view-model processing WS protocol frames and emitting scene graphs (SPEC §12.3; ADR-0044).
//!
//! # SPEC §2 Normative Landmark ↔ Directory Mapping Table
//!
//! | Landmark ID | Landmark Name      | SPEC §2 Tree Path         | Description & State Invariants                 |
//! |-------------|--------------------|---------------------------|------------------------------------------------|
//! | `guild`     | Guild Hall         | `guild/`                  | Commissions ledger, register, blueprints       |
//! | `courier`   | Courier Post       | `courier/`                | Mail spools (/var/mail/), freight, dispatch    |
//! | `keep`      | Keep & Vault       | `keep/`                   | Promoted deliverables (/vault), archives       |
//! | `tavern`    | Tavern & Stasis    | `tavern/`                 | Hibernated patrons (/patrons), user gossip     |
//! | `market`    | Market Stalls      | `market/`                 | Skills repository (/skills), package cache     |
//! | `manor`     | Mayor's Manor      | `keep/census/manor/`      | Mayor orchestrator home (/home/1000/), decrees |
//! | `parcel*`   | Census Workshops   | `keep/census/parcel<uid>/`| Active agent workspaces, deeds, titles         |
//!
//! # Rendering & Performance Policy (Frame-Budget Guard)
//!
//! Target: 60 FPS (16.6ms budget).
//! When client blitting indicates frame budget degradation (`performance_degraded = true`):
//! - Policy degrades over-the-head chat bubbles first:
//!   1. Bubble display timeouts are accelerated (2x decay rate).
//!   2. Concurrent active bubbles rendered are clamped to at most 2.
//!   3. High-priority bubbles (escalations, operator steers) take precedence over speech/mail.
//! - Sprites, landmarks, workshop structures, and HUD metrics are strictly preserved.

use std::collections::{BTreeMap, VecDeque};

use hamlet_core::api::{
    ApiCensusCounters, BubbleFrame, BubbleKind, BubbleTarget, CensusRecordView,
    CommissionSummaryView, DecreeSummaryView, EscalationView, LandmarksView, MetricsBatchFrame,
    ParcelMetricTick, StateDeltaFrame, VaultItemView, WsServerFrame, MAX_MAIL_SNIPPET_LEN,
};

use crate::panels::{
    ActivePanel, AgentPanelView, CourierPanelView, DecreeDeskView, GuildBoardView,
    IndentureEntryView, MutationStatus, OptimisticMutation, PanelKind, PatronView, TavernPanelView,
    VaultPanelView,
};
use crate::scene::{
    HudData, RenderBubble, RenderLandmark, RenderSprite, RenderWorkshop, SceneGraph,
};
use crate::tiles::{Point, VillageMapData};

/// Maximum history entries kept per-agent for sparklines.
pub const SPARKLINE_HISTORY_LEN: usize = 12;

/// Maximum entries kept in the village-wide ticker tape buffer.
pub const TICKER_HISTORY_CAP: usize = 20;

/// Active chat bubble tracked with on-screen lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBubble {
    pub frame: BubbleFrame,
    pub remaining_ms: u32,
}

/// Pure state machine representing the observable village state.
#[derive(Debug, Clone, PartialEq)]
pub struct VillageViewModel {
    pub map: VillageMapData,
    pub daemon_instance_id: Option<String>,
    pub last_seq: u64,
    pub census: BTreeMap<u32, CensusRecordView>,
    pub commissions: Vec<CommissionSummaryView>,
    pub decrees: Vec<DecreeSummaryView>,
    pub vault: Vec<VaultItemView>,
    pub mail_counts: BTreeMap<u32, usize>,
    pub landmarks: LandmarksView,
    pub escalations: Vec<EscalationView>,
    pub metrics: BTreeMap<u32, ParcelMetricTick>,
    /// Recent metric history per UID for sparkline rendering.
    pub metric_history: BTreeMap<u32, Vec<u64>>,
    /// Currently displayed bubble per target ID ("agent-1001", "mayor", "guild", etc.).
    pub active_bubbles: BTreeMap<String, ActiveBubble>,
    /// Pending bubble queues per target ID (FIFO queueing).
    pub bubble_queues: BTreeMap<String, VecDeque<BubbleFrame>>,
    /// Agent UIDs that have received steer directives (maps UID -> remaining ms of attention mark).
    pub steered_uids: BTreeMap<u32, u32>,
    /// Village-wide ticker tape buffer.
    pub ticker_history: VecDeque<String>,
    /// Animation tick counter for cycling sprite animation frames.
    pub tick_counter: u32,
    /// Frame-budget degradation guard flag (60fps target; degrades bubbles first).
    pub performance_degraded: bool,
    /// Currently opened work management panel.
    pub current_panel: Option<PanelKind>,
    /// Target entity identifier for the currently opened panel (e.g. "1001", "guild").
    pub current_panel_target: Option<String>,
    /// In-flight optimistic mutations awaiting server acknowledgment.
    pub pending_mutations: BTreeMap<String, OptimisticMutation>,
    /// Last error received from a rejected command acknowledgment.
    pub last_mutation_error: Option<String>,
    /// Flag indicating a sequence gap or resync was detected and snapshot resync is required.
    pub resync_needed: bool,
}

impl Default for VillageViewModel {
    fn default() -> Self {
        Self::new()
    }
}

impl VillageViewModel {
    /// Creates a new village view model initialized with the canonical village map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: VillageMapData::load_default(),
            daemon_instance_id: None,
            last_seq: 0,
            census: BTreeMap::new(),
            commissions: Vec::new(),
            decrees: Vec::new(),
            vault: Vec::new(),
            mail_counts: BTreeMap::new(),
            landmarks: LandmarksView::default(),
            escalations: Vec::new(),
            metrics: BTreeMap::new(),
            metric_history: BTreeMap::new(),
            active_bubbles: BTreeMap::new(),
            bubble_queues: BTreeMap::new(),
            steered_uids: BTreeMap::new(),
            ticker_history: VecDeque::new(),
            tick_counter: 0,
            performance_degraded: false,
            current_panel: None,
            current_panel_target: None,
            pending_mutations: BTreeMap::new(),
            last_mutation_error: None,
            resync_needed: false,
        }
    }

    /// Appends a message to the village-wide ticker tape buffer.
    pub fn push_ticker(&mut self, text: impl Into<String>) {
        if self.ticker_history.len() >= TICKER_HISTORY_CAP {
            self.ticker_history.pop_front();
        }
        self.ticker_history.push_back(text.into());
    }

    /// Sets the frame-budget degradation guard state.
    pub fn set_degraded_mode(&mut self, degraded: bool) {
        self.performance_degraded = degraded;
    }

    /// Returns true if a sequence gap or resync requirement was detected.
    #[must_use]
    pub fn resync_needed(&self) -> bool {
        self.resync_needed
    }

    /// Clears the resync needed flag.
    pub fn clear_resync_needed(&mut self) {
        self.resync_needed = false;
    }

    /// Ingests a raw server-to-client WS frame. Returns true if scene state changed.
    pub fn apply_server_frame(&mut self, frame: WsServerFrame) -> bool {
        // Sequence gap detection (SPEC §12.3; ADR-0044 cl. 2 & QA09-03)
        let (incoming_seq, is_snapshot) = match &frame {
            WsServerFrame::StateDelta(delta) => (Some(delta.seq), delta.is_snapshot),
            WsServerFrame::Metrics(batch) => (Some(batch.seq), false),
            WsServerFrame::Bubble(bubble) => (Some(bubble.seq), false),
            WsServerFrame::CommandAck(ack) => (Some(ack.seq), false),
            WsServerFrame::ResyncRequired { seq, .. } => (Some(*seq), false),
            _ => (None, false),
        };

        if let Some(seq) = incoming_seq {
            if is_snapshot {
                self.resync_needed = false;
            } else if self.last_seq > 0 && seq > self.last_seq + 1 {
                self.resync_needed = true;
                self.push_ticker(format!(
                    "[GAP] Sequence gap detected (seq {} -> {}), resync required",
                    self.last_seq, seq
                ));
            }
        }

        match frame {
            WsServerFrame::StateDelta(delta) => {
                self.apply_state_delta(*delta);
                true
            }
            WsServerFrame::Metrics(batch) => {
                self.apply_metrics(batch);
                true
            }
            WsServerFrame::Bubble(bubble) => {
                self.apply_bubble(bubble);
                true
            }
            WsServerFrame::ResyncRequired {
                seq,
                daemon_instance_id,
                reason,
                ..
            } => {
                self.last_seq = seq;
                self.daemon_instance_id = Some(daemon_instance_id);
                // Clear all state caches on resync required to guarantee zero ghost state across daemon restarts.
                self.census.clear();
                self.commissions.clear();
                self.decrees.clear();
                self.vault.clear();
                self.mail_counts.clear();
                self.escalations.clear();
                self.metrics.clear();
                self.metric_history.clear();
                self.active_bubbles.clear();
                self.bubble_queues.clear();
                self.steered_uids.clear();
                self.resync_needed = true;
                self.push_ticker(format!("[RESYNC] Cache flushed: {reason}"));
                true
            }
            WsServerFrame::CommandAck(ack) => {
                let success = ack.success;
                let err_opt = ack.error.clone();
                let correlation_id = ack.correlation_id.clone();
                let command_endpoint = ack.command_endpoint.clone();
                let status_code = ack.status_code;

                if let Some(mutation) = self.pending_mutations.get_mut(&correlation_id) {
                    let desc = mutation.description.clone();
                    if success {
                        mutation.status = MutationStatus::Confirmed;
                        self.last_mutation_error = None;
                        self.push_ticker(format!("[ACK OK] {} ({})", desc, command_endpoint));
                    } else {
                        mutation.status = MutationStatus::RolledBack;
                        mutation.error_message = err_opt.clone();
                        let msg = err_opt.unwrap_or_else(|| "Command rejected by server".into());
                        self.last_mutation_error = Some(msg.clone());
                        self.push_ticker(format!(
                            "[ROLLBACK] {} (code {}): {}",
                            command_endpoint, status_code, msg
                        ));
                    }
                } else if let Some(err) = &ack.error {
                    self.last_mutation_error = Some(err.clone());
                    self.push_ticker(format!(
                        "[ACK ERR] {} (code {}): {}",
                        command_endpoint, status_code, err
                    ));
                } else {
                    self.last_mutation_error = None;
                    self.push_ticker(format!("[ACK] {} OK", command_endpoint));
                }
                true
            }
            WsServerFrame::Event(ev) => {
                self.push_ticker(format!("[EVENT] {}: {}", ev.topic, ev.event_kind));
                true
            }
            WsServerFrame::Pong { .. } => false,
        }
    }

    /// Applies projection state snapshot or incremental diff.
    pub fn apply_state_delta(&mut self, delta: StateDeltaFrame) {
        // Restart detection: if instance id changes, flush caches to guarantee no ghost projections.
        if let Some(existing_id) = &self.daemon_instance_id {
            if *existing_id != delta.daemon_instance_id {
                self.census.clear();
                self.commissions.clear();
                self.decrees.clear();
                self.vault.clear();
                self.mail_counts.clear();
                self.escalations.clear();
                self.metrics.clear();
                self.metric_history.clear();
                self.active_bubbles.clear();
                self.bubble_queues.clear();
                self.steered_uids.clear();
                self.push_ticker(format!(
                    "[RESTART] Switched daemon instance: {}",
                    delta.daemon_instance_id
                ));
            }
        }
        self.daemon_instance_id = Some(delta.daemon_instance_id);
        self.last_seq = delta.seq;

        if delta.is_snapshot {
            self.resync_needed = false;
            self.census.clear();
            if let Some(census_vec) = delta.census {
                for rec in census_vec {
                    self.census.insert(rec.uid, rec);
                }
            }
            self.commissions = delta.commissions.unwrap_or_default();
            self.decrees = delta.decrees.unwrap_or_default();
            self.vault = delta.vault.unwrap_or_default();
            self.mail_counts = delta.mail_counts.unwrap_or_default();
            if let Some(lm) = delta.landmarks {
                self.landmarks = lm;
            }
            self.escalations = delta.escalations.unwrap_or_default();
            self.push_ticker(format!(
                "[SNAPSHOT] seq={} | {} agents | {} commissions | {} vault items",
                self.last_seq,
                self.census.len(),
                self.commissions.len(),
                self.vault.len()
            ));
        } else {
            // Incremental diff
            if let Some(census_vec) = delta.census {
                for rec in census_vec {
                    self.census.insert(rec.uid, rec);
                }
            }
            if let Some(commissions) = delta.commissions {
                self.commissions = commissions;
            }
            if let Some(decrees) = delta.decrees {
                self.decrees = decrees;
            }
            if let Some(vault) = delta.vault {
                self.vault = vault;
            }
            if let Some(mail_counts) = delta.mail_counts {
                for (uid, count) in mail_counts {
                    self.mail_counts.insert(uid, count);
                }
            }
            if let Some(lm) = delta.landmarks {
                self.landmarks = lm;
            }
            if let Some(escalations) = delta.escalations {
                self.escalations = escalations;
            }
        }
    }

    /// Applies incoming batch of metrics ticks.
    pub fn apply_metrics(&mut self, batch: MetricsBatchFrame) {
        self.last_seq = batch.seq;
        let mut total_batch_tokens = 0u64;
        let mut total_batch_cost = 0u64;

        for tick in batch.ticks {
            let combined_tokens = tick.tokens_in + tick.tokens_out;
            total_batch_tokens += combined_tokens;
            if let Some(cost) = tick.cost_micro_usd {
                total_batch_cost += cost;
            }

            // Record per-sprite sparkline history
            let hist = self.metric_history.entry(tick.uid).or_default();
            hist.push(combined_tokens);
            if hist.len() > SPARKLINE_HISTORY_LEN {
                hist.remove(0);
            }

            self.metrics.insert(tick.uid, tick);
        }

        let cost_dollars = (total_batch_cost as f64) / 1_000_000.0;
        self.push_ticker(format!(
            "[METRICS] seq={} | {} parcels reporting | tokens: {} | cost: ${:.4}",
            batch.seq,
            self.metrics.len(),
            total_batch_tokens,
            cost_dollars
        ));
    }

    /// Enqueues an over-the-head chat bubble with client-side honesty rule enforcement and queueing.
    pub fn apply_bubble(&mut self, mut bubble: BubbleFrame) {
        self.last_seq = bubble.seq;

        // Client-side honesty display rule enforcement (SPEC §12.3 Defense in Depth):
        // Cross-owner mail bodies are strictly suppressed on the spectator wire. Even if an un-sanitized
        // body or excessive length were provided, the client display rule enforces subject + bounded snippet.
        if let BubbleKind::MailSnippet {
            subject, snippet, ..
        } = &bubble.kind
        {
            let bounded_snippet = if snippet.chars().count() > MAX_MAIL_SNIPPET_LEN {
                let s: String = snippet.chars().take(MAX_MAIL_SNIPPET_LEN).collect();
                format!("{s}...")
            } else {
                snippet.clone()
            };
            bubble.text = format!("[Mail] {subject}: {bounded_snippet}");
        }

        let (target_key, target_uid) = match &bubble.target {
            BubbleTarget::Agent(uid) => (format!("agent-{uid}"), Some(*uid)),
            BubbleTarget::Mayor => ("mayor".to_string(), Some(1000)),
            BubbleTarget::Landmark(id) => (id.clone(), None),
            BubbleTarget::Broadcast => ("broadcast".to_string(), None),
        };

        // If this is a steer confirmation, mark the agent as steered with an attention mark timer
        if let BubbleKind::SteerConfirmation { uid, .. } = &bubble.kind {
            self.steered_uids.insert(*uid, 4000);
        } else if let Some(uid) = target_uid {
            if bubble.text.starts_with("Steered:") {
                self.steered_uids.insert(uid, 4000);
            }
        }

        self.push_ticker(format!("[BUBBLE] {}: {}", target_key, bubble.text));

        // Queueing: if an active bubble is currently displaying for this target, append to target's FIFO queue
        match self.active_bubbles.entry(target_key.clone()) {
            std::collections::btree_map::Entry::Occupied(_) => {
                let queue = self.bubble_queues.entry(target_key).or_default();
                // Cap queue per target to 8 to avoid unbounded backlog
                if queue.len() < 8 {
                    queue.push_back(bubble);
                }
            }
            std::collections::btree_map::Entry::Vacant(v) => {
                let remaining_ms = bubble.duration_ms.max(1000);
                v.insert(ActiveBubble {
                    frame: bubble,
                    remaining_ms,
                });
            }
        }
    }

    /// Advances animation cycles and bubble timers by `dt_ms` milliseconds.
    pub fn tick(&mut self, dt_ms: u32) {
        self.tick_counter = self.tick_counter.wrapping_add(1);

        // Decay steered attention mark timers
        for rem in self.steered_uids.values_mut() {
            *rem = rem.saturating_sub(dt_ms);
        }
        self.steered_uids.retain(|_, rem| *rem > 0);

        // Frame-budget guard: if performance is degraded, accelerate bubble timeout (2x speed)
        let bubble_decay_ms = if self.performance_degraded {
            dt_ms.saturating_mul(2)
        } else {
            dt_ms
        };

        let mut expired_keys = Vec::new();
        for (key, active) in &mut self.active_bubbles {
            active.remaining_ms = active.remaining_ms.saturating_sub(bubble_decay_ms);
            if active.remaining_ms == 0 {
                expired_keys.push(key.clone());
            }
        }

        // Pop next queued bubble for each expired target
        for key in expired_keys {
            if let Some(queue) = self.bubble_queues.get_mut(&key) {
                if let Some(next_frame) = queue.pop_front() {
                    let remaining_ms = next_frame.duration_ms.max(1000);
                    self.active_bubbles.insert(
                        key,
                        ActiveBubble {
                            frame: next_frame,
                            remaining_ms,
                        },
                    );
                    continue;
                }
            }
            self.active_bubbles.remove(&key);
        }
    }

    /// Renders the current state into a pure scene graph for canvas presentation.
    #[must_use]
    pub fn generate_scene(&self) -> SceneGraph {
        let mut scene =
            SceneGraph::empty(self.map.map_width, self.map.map_height, self.map.tile_size);

        // 1. Render landmarks (SPEC §2 buildings)
        for def in &self.map.landmarks {
            let (item_count, status_label) = match def.id.as_str() {
                "guild" => (
                    self.commissions.len(),
                    format!("{} active commissions", self.commissions.len()),
                ),
                "courier" => {
                    let total_mail: usize = self.mail_counts.values().sum();
                    (total_mail, format!("{total_mail} unread spools"))
                }
                "keep" => (
                    self.vault.len(),
                    format!("{} deliverables", self.vault.len()),
                ),
                "tavern" => {
                    let stasis_count = self.census.values().filter(|a| a.stasis).count();
                    (stasis_count, format!("{stasis_count} patrons resting"))
                }
                "market" => (
                    self.landmarks.market.skills.len(),
                    format!("{} skills cached", self.landmarks.market.skills.len()),
                ),
                "manor" => {
                    let pending = self
                        .decrees
                        .iter()
                        .filter(|d| d.status == "pending" || d.status == "staged")
                        .count();
                    (pending, format!("{pending} decrees pending"))
                }
                _ => (0, String::new()),
            };

            scene.landmarks.push(RenderLandmark {
                id: def.id.clone(),
                name: def.name.clone(),
                tree_path: def.tree_path.clone(),
                x: def.bounds.x,
                y: def.bounds.y,
                w: def.bounds.w,
                h: def.bounds.h,
                color: def.color.clone(),
                item_count,
                status_label,
            });
        }

        // 2. Render census workshop structures (one per active parcel in SPEC §2 keep/census/parcel<uid>/)
        for (uid, record) in &self.census {
            if *uid == 1000 {
                continue; // Mayor occupies Manor, not a parcel workshop
            }
            let pos = self
                .map
                .slot_for_uid(*uid)
                .unwrap_or(Point { x: 16, y: 12 });
            let tree_path = format!("keep/census/parcel{uid}/");
            let status_label = format!("parcel{} [{}]: {}", uid, record.state, record.identity);

            scene.workshops.push(RenderWorkshop {
                uid: *uid,
                parcel_slug: format!("parcel{uid}"),
                tree_path,
                x: pos.x.saturating_sub(1),
                y: pos.y.saturating_sub(1),
                w: 3,
                h: 3,
                agent_name: record.identity.as_str().to_string(),
                state: record.state.clone(),
                status_label,
            });
        }

        // 3. Render Mayor sprite (UID 1000) at Manor (SPEC §2 keep/census/manor/)
        let mayor_metric = self.metrics.get(&1000);
        let mayor_sparkline = self.metric_history.get(&1000).cloned().unwrap_or_default();
        scene.sprites.push(RenderSprite {
            uid: 1000,
            name: "Mayor".into(),
            x: 27.0,
            y: 19.0,
            state: "live".into(),
            char_col: 0,
            char_row: 1, // Distinct sprite row for Mayor
            is_mayor: true,
            tokens_in: mayor_metric.map_or(0, |m| m.tokens_in),
            tokens_out: mayor_metric.map_or(0, |m| m.tokens_out),
            cost_micro_usd: mayor_metric.and_then(|m| m.cost_micro_usd).unwrap_or(0),
            anim_state: "live".into(),
            anim_frame: (self.tick_counter / 4) % 2,
            tool_swing: false,
            loiter: true,
            zzz: false,
            fallen: false,
            attention_mark: false,
            sparkline: mayor_sparkline,
        });

        // 4. Render agent sprites in parcel workshops (derived from state-delta frames ONLY)
        for (uid, record) in &self.census {
            if *uid == 1000 {
                continue;
            }
            let pos = self
                .map
                .slot_for_uid(*uid)
                .unwrap_or(Point { x: 16, y: 12 });
            let metric = self.metrics.get(uid);
            let sparkline = self.metric_history.get(uid).cloned().unwrap_or_default();

            // State-driven animation mapping per prompt:
            // working = tool-swing, idle = loiter, sleeping/hibernated = zzz, crashed = fallen, steered = attention mark
            let is_steered = self.steered_uids.contains_key(uid);

            let (anim_state, tool_swing, loiter, zzz, fallen, char_col, char_row) =
                match record.state.as_str() {
                    "live" | "working" => {
                        let frame = (self.tick_counter / 2) % 4;
                        ("working".into(), true, false, false, false, frame, 0)
                    }
                    "idle" => {
                        let frame = (self.tick_counter / 3) % 2;
                        ("idle".into(), false, true, false, false, 1 + frame, 0)
                    }
                    "stasis" | "sleeping" => ("sleeping".into(), false, false, true, false, 3, 0),
                    "crashed" | "stopped" => ("crashed".into(), false, false, false, true, 4, 0),
                    _ => ("idle".into(), false, true, false, false, 0, 0),
                };

            let final_anim_state = if is_steered {
                "steered".into()
            } else {
                anim_state
            };

            scene.sprites.push(RenderSprite {
                uid: *uid,
                name: record.identity.as_str().to_string(),
                x: pos.x as f32,
                y: pos.y as f32,
                state: record.state.clone(),
                char_col,
                char_row,
                is_mayor: false,
                tokens_in: metric.map_or(0, |m| m.tokens_in),
                tokens_out: metric.map_or(0, |m| m.tokens_out),
                cost_micro_usd: metric.and_then(|m| m.cost_micro_usd).unwrap_or(0),
                anim_state: final_anim_state,
                anim_frame: self.tick_counter % 4,
                tool_swing,
                loiter,
                zzz: zzz || record.stasis,
                fallen,
                attention_mark: is_steered,
                sparkline,
            });
        }

        // 5. Render active chat bubbles over sprite heads (with frame-budget guard degradation)
        let mut active_bubble_list: Vec<&ActiveBubble> = self.active_bubbles.values().collect();

        // Frame-budget guard: if degraded, clamp rendered bubbles to 2, prioritizing critical bubbles
        if self.performance_degraded && active_bubble_list.len() > 2 {
            active_bubble_list.sort_by_key(|b| match &b.frame.kind {
                BubbleKind::EscalationNotice { .. } => 0,
                BubbleKind::SteerConfirmation { .. } => 1,
                BubbleKind::DecreeAck { .. } => 2,
                BubbleKind::MailSnippet { .. } => 3,
                BubbleKind::AgentSpeech { .. } => 4,
            });
            active_bubble_list.truncate(2);
        }

        for active in active_bubble_list {
            let (bx, by, target_uid, target_id) = match &active.frame.target {
                BubbleTarget::Agent(uid) => {
                    if *uid == 1000 {
                        (27.0, 18.0, Some(1000), "mayor".into())
                    } else {
                        let pt = self
                            .map
                            .slot_for_uid(*uid)
                            .unwrap_or(Point { x: 16, y: 12 });
                        (
                            pt.x as f32,
                            (pt.y as f32) - 1.2,
                            Some(*uid),
                            format!("agent-{uid}"),
                        )
                    }
                }
                BubbleTarget::Mayor => (27.0, 18.0, Some(1000), "mayor".into()),
                BubbleTarget::Landmark(id) => {
                    if let Some(lm) = self.map.find_landmark(id) {
                        (
                            lm.interact_point.x as f32,
                            (lm.interact_point.y as f32) - 1.0,
                            None,
                            id.clone(),
                        )
                    } else {
                        (16.0, 10.0, None, id.clone())
                    }
                }
                BubbleTarget::Broadcast => (16.0, 8.0, None, "broadcast".into()),
            };

            let is_mail = matches!(&active.frame.kind, BubbleKind::MailSnippet { .. });
            let kind_str = match &active.frame.kind {
                BubbleKind::MailSnippet { .. } => "mail",
                BubbleKind::SteerConfirmation { .. } => "steer",
                BubbleKind::EscalationNotice { .. } => "escalation",
                BubbleKind::DecreeAck { .. } => "decree_ack",
                BubbleKind::AgentSpeech { .. } => "speech",
            };

            scene.bubbles.push(RenderBubble {
                target_uid,
                target_id,
                x: bx,
                y: by,
                text: active.frame.text.clone(),
                kind: kind_str.into(),
                remaining_ms: active.remaining_ms,
                is_mail,
            });
        }

        // 6. Construct HUD summary and ticker tape
        let total_cost: u64 = self.metrics.values().filter_map(|m| m.cost_micro_usd).sum();
        let unread_mail: usize = self.mail_counts.values().sum();
        let pending_decrees = self
            .decrees
            .iter()
            .filter(|d| d.status == "pending" || d.status == "staged")
            .count();

        let ticker: Vec<String> = self.ticker_history.iter().rev().take(5).cloned().collect();

        scene.hud = HudData {
            daemon_instance_id: self.daemon_instance_id.clone().unwrap_or_default(),
            seq: self.last_seq,
            active_agents: self
                .census
                .values()
                .filter(|a| !a.stasis && a.uid != 1000)
                .count(),
            active_commissions: self
                .commissions
                .iter()
                .filter(|c| c.state == "Active")
                .count(),
            pending_decrees,
            unread_mail,
            total_cost_micro_usd: total_cost,
            ticker,
            frame_budget_degraded: self.performance_degraded,
        };

        scene
    }

    /// Opens a work management panel with an optional target identifier.
    pub fn open_panel(&mut self, kind: PanelKind, target: Option<String>) {
        self.current_panel = Some(kind);
        self.current_panel_target = target;
        self.push_ticker(format!("[PANEL] Opened {:?}", kind));
    }

    /// Closes the active work management panel.
    pub fn close_panel(&mut self) {
        self.current_panel = None;
        self.current_panel_target = None;
    }

    /// Records an optimistic in-flight mutation command awaiting server ACK.
    pub fn record_optimistic_command(
        &mut self,
        correlation_id: impl Into<String>,
        description: impl Into<String>,
        target: impl Into<String>,
    ) {
        let cid = correlation_id.into();
        let desc = description.into();
        let tgt = target.into();
        self.pending_mutations.insert(
            cid.clone(),
            OptimisticMutation {
                correlation_id: cid,
                description: desc.clone(),
                target: tgt,
                timestamp_unix_ms: self.last_seq * 1000,
                status: MutationStatus::Pending,
                error_message: None,
            },
        );
        self.push_ticker(format!("[CMD PENDING] {desc}"));
    }

    /// Generates the pure projection model for the currently active management panel.
    #[must_use]
    pub fn generate_panel_view(&self) -> ActivePanel {
        let kind = match self.current_panel {
            Some(k) => k,
            None => return ActivePanel::None,
        };

        match kind {
            PanelKind::Agent => {
                let target_uid: u32 = self
                    .current_panel_target
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1001);

                if target_uid == 1000 {
                    let metric = self.metrics.get(&1000);
                    return ActivePanel::Agent(AgentPanelView {
                        uid: 1000,
                        name: "Mayor".into(),
                        state: "live".into(),
                        stasis: false,
                        commission_id: None,
                        commission_slug: None,
                        session_count: 1,
                        counters: ApiCensusCounters::default(),
                        tokens_in: metric.map_or(0, |m| m.tokens_in),
                        tokens_out: metric.map_or(0, |m| m.tokens_out),
                        cost_micro_usd: metric.and_then(|m| m.cost_micro_usd).unwrap_or(0),
                        latency_ms: metric.map_or(0, |m| m.latency_ms),
                        memory_bytes: metric.and_then(|m| m.memory_bytes).unwrap_or(0),
                        cpu_percent_x10: metric.and_then(|m| m.cpu_percent_x10).unwrap_or(0),
                        title_digest: "mayor-trunk-exec".into(),
                        lineage: vec![
                            "Permanent slot 1000".into(),
                            "Stateless executive reconstitution engine".into(),
                        ],
                        deed_summary: "Mayor's Manor: keep/census/manor/DEED.md".into(),
                        proprietor_summary: "Trunk prompt: keep/census/manor/MAYOR.md".into(),
                        can_steer: false,
                        can_hibernate: false,
                        can_wake: false,
                    });
                }

                if let Some(record) = self.census.get(&target_uid) {
                    let metric = self.metrics.get(&target_uid);
                    let mut lineage = vec![
                        format!("Identity: {}", record.identity),
                        format!("State: {}", record.state),
                        format!("Title digest: {}", record.title_digest),
                    ];
                    if let Some(c) = record.commission {
                        lineage.push(format!("Assigned to commission #{c}"));
                    }
                    if let Some(ev) = &record.last_event {
                        lineage.push(format!("Last event: {ev}"));
                    }

                    ActivePanel::Agent(AgentPanelView {
                        uid: target_uid,
                        name: record.identity.as_str().to_string(),
                        state: record.state.clone(),
                        stasis: record.stasis,
                        commission_id: record.commission,
                        commission_slug: record.commission_slug.as_ref().map(|s| s.to_string()),
                        session_count: record.session_count,
                        counters: record.counters,
                        tokens_in: metric.map_or(record.counters.input_tokens, |m| m.tokens_in),
                        tokens_out: metric.map_or(record.counters.output_tokens, |m| m.tokens_out),
                        cost_micro_usd: metric
                            .and_then(|m| m.cost_micro_usd)
                            .unwrap_or(record.counters.cost_micro_usd),
                        latency_ms: metric.map_or(0, |m| m.latency_ms),
                        memory_bytes: metric.and_then(|m| m.memory_bytes).unwrap_or(0),
                        cpu_percent_x10: metric.and_then(|m| m.cpu_percent_x10).unwrap_or(0),
                        title_digest: record.title_digest.clone(),
                        lineage,
                        deed_summary: format!("keep/census/parcel{target_uid}/DEED.md"),
                        proprietor_summary: format!(
                            "keep/census/parcel{target_uid}/PROPRIETOR.md (Sessions: {})",
                            record.session_count
                        ),
                        can_steer: !record.stasis && record.state != "crashed",
                        can_hibernate: !record.stasis && record.state != "crashed",
                        can_wake: record.stasis,
                    })
                } else {
                    ActivePanel::None
                }
            }
            PanelKind::Guild => {
                let active_count = self
                    .commissions
                    .iter()
                    .filter(|c| c.state == "Active")
                    .count();
                let draft_count = self
                    .commissions
                    .iter()
                    .filter(|c| c.state == "draft" || c.state == "Draft")
                    .count();
                let closed_count = self
                    .commissions
                    .iter()
                    .filter(|c| c.state == "closed" || c.state == "Closed")
                    .count();

                let mut indenture_entries = Vec::new();
                for (uid, record) in &self.census {
                    if *uid == 1000 {
                        continue;
                    }
                    indenture_entries.push(IndentureEntryView {
                        uid: *uid,
                        agent_name: record.identity.as_str().to_string(),
                        goal: record
                            .commission_slug
                            .as_ref()
                            .map_or("unassigned".into(), |s| s.as_str().to_string()),
                        status: record.state.clone(),
                    });
                }

                ActivePanel::Guild(GuildBoardView {
                    commissions: self.commissions.clone(),
                    active_count,
                    draft_count,
                    closed_count,
                    registered_agents: self.landmarks.guild.register.clone(),
                    blueprints: self.landmarks.guild.blueprints.clone(),
                    indenture_entries,
                    can_create: true,
                    can_close: active_count > 0,
                })
            }
            PanelKind::Vault => {
                let total_bytes: u64 = self.vault.iter().map(|a| a.size_bytes).sum();
                ActivePanel::Vault(VaultPanelView {
                    artifacts: self.vault.clone(),
                    total_count: self.vault.len(),
                    total_bytes,
                    can_export: !self.vault.is_empty(),
                })
            }
            PanelKind::DecreeDesk => {
                let pending_count = self
                    .decrees
                    .iter()
                    .filter(|d| d.status == "pending" || d.status == "staged")
                    .count();
                let can_replay = self.decrees.iter().any(|d| d.status == "rejected");
                ActivePanel::DecreeDesk(DecreeDeskView {
                    decrees: self.decrees.clone(),
                    pending_count,
                    escalations: self.escalations.clone(),
                    can_replay,
                    can_resolve: !self.escalations.is_empty(),
                })
            }
            PanelKind::Courier => {
                let total_unread = self.mail_counts.values().sum();
                ActivePanel::Courier(CourierPanelView {
                    mail_counts: self.mail_counts.clone(),
                    total_unread,
                    freight_sandboxes_count: self.landmarks.courier.freight_sandboxes,
                    dispatch_outbox_count: self.landmarks.courier.dispatch_outbox,
                    dispatch_inbox_count: self.landmarks.courier.dispatch_inbox,
                    can_compose: true,
                })
            }
            PanelKind::Tavern => {
                let patrons: Vec<PatronView> = self
                    .census
                    .values()
                    .filter(|a| a.stasis && a.uid != 1000)
                    .map(|a| PatronView {
                        uid: a.uid,
                        identity: a.identity.clone(),
                        last_commission: a.commission_slug.as_ref().map(|s| s.to_string()),
                        title_digest: a.title_digest.clone(),
                    })
                    .collect();
                let stasis_count = patrons.len();
                ActivePanel::Tavern(TavernPanelView {
                    patrons,
                    stasis_count,
                    gossip_files: self.landmarks.tavern.gossip.clone(),
                    can_awaken: stasis_count > 0,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamlet_core::api::ApiCensusCounters;
    use hamlet_core::sanitize::sanitize_name;

    fn safe(s: &str) -> hamlet_core::sanitize::SafeName {
        sanitize_name(s).expect("valid safe name")
    }

    #[test]
    fn view_model_generates_scene_from_state_delta() {
        let mut vm = VillageViewModel::new();
        assert_eq!(vm.last_seq, 0);

        let mut mail_counts = BTreeMap::new();
        mail_counts.insert(1001, 2);

        let delta = StateDeltaFrame::snapshot(
            "inst-1",
            1,
            1729000000000,
            vec![CensusRecordView {
                uid: 1001,
                identity: safe("worker-1"),
                commission: Some(1),
                commission_slug: Some(safe("demo")),
                state: "live".into(),
                stasis: false,
                session_count: 2,
                counters: ApiCensusCounters {
                    input_tokens: 1000,
                    output_tokens: 200,
                    cost_micro_usd: 5000,
                    traces: 10,
                },
                last_event: Some("pi_tool_call".into()),
                last_event_ts: Some("1729000000000".into()),
                title_digest: "digest".into(),
            }],
            vec![CommissionSummaryView {
                commission_id: 1,
                slug: safe("demo"),
                state: "Active".into(),
                roster: vec![1001],
                deliverables_count: 0,
                brief: true,
                indenture: true,
                git: "init".into(),
            }],
            vec![],
            vec![],
            mail_counts,
            LandmarksView::default(),
            vec![],
        );

        vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));
        assert_eq!(vm.last_seq, 1);
        assert_eq!(vm.daemon_instance_id.as_deref(), Some("inst-1"));

        let scene = vm.generate_scene();
        assert_eq!(scene.hud.active_agents, 1);
        assert_eq!(scene.hud.active_commissions, 1);
        assert_eq!(scene.hud.unread_mail, 2);

        // Census workshop rendered for active parcel 1001
        assert_eq!(scene.workshops.len(), 1);
        assert_eq!(scene.workshops[0].uid, 1001);
        assert_eq!(scene.workshops[0].tree_path, "keep/census/parcel1001/");

        // Mayor is always rendered at Manor
        assert!(scene.sprites.iter().any(|s| s.is_mayor && s.uid == 1000));
        // Worker-1 is rendered at slot 1001 (10, 10)
        let worker = scene
            .sprites
            .iter()
            .find(|s| s.uid == 1001)
            .expect("worker 1001");
        assert_eq!(worker.name, "worker-1");
        assert_eq!(worker.x, 10.0);
        assert_eq!(worker.y, 10.0);
        assert_eq!(worker.state, "live");
        assert!(worker.tool_swing);
        assert_eq!(worker.anim_state, "working");
    }

    #[test]
    fn view_model_bubbles_queue_and_expire_on_tick() {
        let mut vm = VillageViewModel::new();
        let bubble1 = BubbleFrame::steer(2, 1729000000000, 1001, "Run verification");
        let bubble2 = BubbleFrame::steer(3, 1729000001000, 1001, "Second instruction");

        vm.apply_server_frame(WsServerFrame::Bubble(bubble1));
        vm.apply_server_frame(WsServerFrame::Bubble(bubble2));

        // Only first bubble active; second is queued for agent-1001
        let scene1 = vm.generate_scene();
        assert_eq!(scene1.bubbles.len(), 1);
        assert!(scene1.bubbles[0].text.contains("Run verification"));

        // Advance timer to expire first bubble (duration = 3000ms)
        vm.tick(3000);
        let scene2 = vm.generate_scene();
        assert_eq!(scene2.bubbles.len(), 1);
        assert!(scene2.bubbles[0].text.contains("Second instruction"));

        // Advance timer to expire second bubble
        vm.tick(3000);
        let scene3 = vm.generate_scene();
        assert_eq!(scene3.bubbles.len(), 0);
    }

    #[test]
    fn view_model_enforces_honesty_rule_client_side() {
        let mut vm = VillageViewModel::new();
        // Construct mail bubble with very long raw snippet exceeding limit
        let long_body = "A".repeat(120);
        let bubble =
            BubbleFrame::mail_snippet(4, 1729000000000, 1000, 1001, "Secret Directive", &long_body);
        vm.apply_server_frame(WsServerFrame::Bubble(bubble));

        let scene = vm.generate_scene();
        assert_eq!(scene.bubbles.len(), 1);
        let b = &scene.bubbles[0];
        assert!(b.is_mail);
        // Client-side display rule clamps snippet to at most MAX_MAIL_SNIPPET_LEN (64)
        assert!(b.text.len() <= 100);
        assert!(b.text.starts_with("[Mail] Secret Directive:"));
    }

    #[test]
    fn view_model_state_driven_animation_states() {
        let mut vm = VillageViewModel::new();
        let records = vec![
            CensusRecordView {
                uid: 1001,
                identity: safe("worker-working"),
                commission: Some(1),
                commission_slug: Some(safe("demo")),
                state: "working".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d1".into(),
            },
            CensusRecordView {
                uid: 1002,
                identity: safe("worker-idle"),
                commission: Some(1),
                commission_slug: Some(safe("demo")),
                state: "idle".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d2".into(),
            },
            CensusRecordView {
                uid: 1003,
                identity: safe("worker-stasis"),
                commission: None,
                commission_slug: None,
                state: "stasis".into(),
                stasis: true,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d3".into(),
            },
            CensusRecordView {
                uid: 1004,
                identity: safe("worker-crashed"),
                commission: None,
                commission_slug: None,
                state: "crashed".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d4".into(),
            },
        ];

        let delta = StateDeltaFrame::snapshot(
            "inst-1",
            1,
            1729000000000,
            records,
            vec![],
            vec![],
            vec![],
            BTreeMap::new(),
            LandmarksView::default(),
            vec![],
        );
        vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

        // Inject steer bubble for worker 1002
        let steer_bubble = BubbleFrame::steer(2, 1729000000000, 1002, "Review tests");
        vm.apply_server_frame(WsServerFrame::Bubble(steer_bubble));

        let scene = vm.generate_scene();
        let s1001 = scene.sprites.iter().find(|s| s.uid == 1001).unwrap();
        assert_eq!(s1001.anim_state, "working");
        assert!(s1001.tool_swing);

        let s1002 = scene.sprites.iter().find(|s| s.uid == 1002).unwrap();
        assert_eq!(s1002.anim_state, "steered");
        assert!(s1002.attention_mark);

        let s1003 = scene.sprites.iter().find(|s| s.uid == 1003).unwrap();
        assert_eq!(s1003.anim_state, "sleeping");
        assert!(s1003.zzz);

        let s1004 = scene.sprites.iter().find(|s| s.uid == 1004).unwrap();
        assert_eq!(s1004.anim_state, "crashed");
        assert!(s1004.fallen);
    }

    #[test]
    fn view_model_frame_budget_guard_degrades_bubbles() {
        let mut vm = VillageViewModel::new();
        // Add 4 bubbles across different targets
        vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::mail_snippet(
            1, 100, 1000, 1001, "m1", "s1",
        )));
        vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::mail_snippet(
            2, 100, 1000, 1002, "m2", "s2",
        )));
        vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::mail_snippet(
            3, 100, 1000, 1003, "m3", "s3",
        )));
        vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::escalation(
            4,
            100,
            "esc-1",
            "OOM error",
        )));

        // Normal mode: all 4 are active
        let normal_scene = vm.generate_scene();
        assert_eq!(normal_scene.bubbles.len(), 4);
        assert!(!normal_scene.hud.frame_budget_degraded);

        // Degraded mode: clamped to 2, escalation prioritized
        vm.set_degraded_mode(true);
        let degraded_scene = vm.generate_scene();
        assert_eq!(degraded_scene.bubbles.len(), 2);
        assert!(degraded_scene.hud.frame_budget_degraded);
        assert!(degraded_scene
            .bubbles
            .iter()
            .any(|b| b.kind == "escalation"));
    }

    #[test]
    fn view_model_resync_clears_ghost_state() {
        let mut vm = VillageViewModel::new();
        let mut mail_counts = BTreeMap::new();
        mail_counts.insert(1001, 5);

        let delta = StateDeltaFrame::snapshot(
            "inst-1",
            1,
            1729000000000,
            vec![],
            vec![],
            vec![],
            vec![],
            mail_counts,
            LandmarksView::default(),
            vec![],
        );
        vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));
        assert_eq!(vm.generate_scene().hud.unread_mail, 5);

        // Daemon restart: new instance triggers cache flush
        vm.apply_server_frame(WsServerFrame::ResyncRequired {
            seq: 10,
            timestamp_unix_ms: 1729000010000,
            daemon_instance_id: "inst-2".into(),
            reason: "daemon restarted".into(),
        });

        let scene = vm.generate_scene();
        assert_eq!(scene.hud.unread_mail, 0);
        assert_eq!(scene.hud.seq, 10);
        assert_eq!(scene.hud.daemon_instance_id, "inst-2");
    }
}
