//! Application lifecycle, event loop, and input handling for `hamlet-tui`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use hamlet_core::api::*;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::client::TuiClient;
use crate::demo::{demo_event_stream, demo_village_data};
use crate::model::*;
use crate::ui::render_app;

/// Options for launching the TUI application.
#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    pub dir: Option<PathBuf>,
    pub endpoint: Option<String>,
    pub token: Option<String>,
    pub initial_room: Option<String>,
    pub demo: bool,
}

pub struct App {
    pub state: AppState,
    pub client: Option<TuiClient>,
    pub ws_rx: mpsc::Receiver<WsEvent>,
    pub ws_tx: mpsc::Sender<WsEvent>,
    pub last_refresh: Instant,
    pub reconnect_attempt: usize,
    pub next_reconnect_at: Option<Instant>,
    pub demo_events: Vec<WsEvent>,
    pub demo_event_idx: usize,
}

impl App {
    pub fn new(options: TuiOptions) -> Result<Self, String> {
        let initial_room = options.initial_room.as_deref().and_then(Room::from_name);
        let mut state = AppState::new(initial_room, options.demo);
        let (ws_tx, ws_rx) = mpsc::channel(256);

        let (client, demo_events) = if options.demo {
            state.data = demo_village_data();
            for ev in demo_event_stream() {
                state.ticker.push(TickerEvent::from(ev));
            }
            (None, demo_event_stream())
        } else {
            let client = match (options.endpoint, options.token) {
                (Some(ep_url), Some(tok)) => {
                    let ep = DaemonEndpoint::new(ep_url, 0, 0, 0);
                    TuiClient::new(ep, LocalToken::new(tok))
                }
                _ => TuiClient::discover(options.dir.as_deref())?,
            };
            (Some(client), Vec::new())
        };

        Ok(Self {
            state,
            client,
            ws_rx,
            ws_tx,
            last_refresh: Instant::now(),
            reconnect_attempt: 0,
            next_reconnect_at: None,
            demo_events,
            demo_event_idx: 0,
        })
    }

    /// Run the interactive TUI event loop on the terminal.
    pub async fn run_loop(&mut self) -> Result<(), String> {
        enable_raw_mode().map_err(|e| format!("enable raw mode: {e}"))?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| format!("setup terminal: {e}"))?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).map_err(|e| format!("terminal init: {e}"))?;

        // Initial refresh and spawn WS listener
        if let Some(client) = self.client.clone() {
            match client.refresh_village_data().await {
                Ok(data) => {
                    self.state.data = data;
                    self.state.connection = ConnectionStatus::Connected;
                }
                Err(err) => {
                    self.schedule_reconnect(err);
                }
            }
            client.spawn_ws_listener(self.ws_tx.clone());
        }

        let res = self.main_loop(&mut terminal).await;

        // Restore terminal
        disable_raw_mode().map_err(|e| format!("disable raw mode: {e}"))?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .map_err(|e| format!("restore terminal: {e}"))?;
        terminal
            .show_cursor()
            .map_err(|e| format!("show cursor: {e}"))?;

        res
    }

    async fn main_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<(), String> {
        let tick_rate = Duration::from_millis(50);

        while !self.state.should_quit {
            terminal
                .draw(|frame| render_app(frame, &self.state))
                .map_err(|e| format!("draw frame: {e}"))?;

            // Drain WebSocket events
            while let Ok(event) = self.ws_rx.try_recv() {
                self.state.ticker.push(TickerEvent::from(event));
            }

            // In demo mode, cycle through demo events periodically
            if self.state.demo_mode
                && !self.demo_events.is_empty()
                && self.last_refresh.elapsed() >= Duration::from_secs(3)
            {
                let ev = self.demo_events[self.demo_event_idx].clone();
                self.state.ticker.push(TickerEvent::from(ev));
                self.demo_event_idx = (self.demo_event_idx + 1) % self.demo_events.len();
                self.last_refresh = Instant::now();
            }

            // Periodic background polling or reconnect handling
            if let Some(client) = &self.client {
                let now = Instant::now();
                if let Some(retry_at) = self.next_reconnect_at {
                    if now >= retry_at {
                        self.next_reconnect_at = None;
                        match client.refresh_village_data().await {
                            Ok(data) => {
                                self.state.data = data;
                                self.state.connection = ConnectionStatus::Connected;
                                self.reconnect_attempt = 0;
                                self.state.set_status("Reconnected to daemon", false);
                                client.spawn_ws_listener(self.ws_tx.clone());
                            }
                            Err(err) => {
                                self.schedule_reconnect(err);
                            }
                        }
                    }
                } else if self.last_refresh.elapsed() >= Duration::from_secs(2) {
                    if let Ok(data) = client.refresh_village_data().await {
                        self.state.data = data;
                        self.state.connection = ConnectionStatus::Connected;
                    } else {
                        self.schedule_reconnect("connection lost".into());
                    }
                    self.last_refresh = now;
                }
            }

            // Process crossterm input events
            if event::poll(tick_rate).map_err(|e| format!("poll event: {e}"))? {
                match event::read().map_err(|e| format!("read event: {e}"))? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key).await;
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    pub fn schedule_reconnect(&mut self, err: String) {
        self.reconnect_attempt += 1;
        let base_delay = 500u64;
        let mult = 1u64
            .checked_shl(self.reconnect_attempt.min(5) as u32)
            .unwrap_or(32);
        let delay_ms = (base_delay * mult).min(10_000);

        self.state.connection = ConnectionStatus::Reconnecting {
            attempt: self.reconnect_attempt,
            next_retry_ms: delay_ms,
            error: err,
        };
        self.next_reconnect_at = Some(Instant::now() + Duration::from_millis(delay_ms));
    }

    pub async fn handle_key(&mut self, key: KeyEvent) {
        // If modal is open, dispatch to modal handler
        if self.state.modal != ModalState::None {
            self.handle_modal_key(key).await;
            return;
        }

        // Global keys
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.state.should_quit = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.should_quit = true;
            }
            // Room cycling
            KeyCode::Tab | KeyCode::Char(']') => {
                self.state.next_room();
            }
            KeyCode::BackTab | KeyCode::Char('[') => {
                self.state.prev_room();
            }
            // Direct room jump 1..=6
            KeyCode::Char('1') => self.state.set_room(Room::Guild),
            KeyCode::Char('2') => self.state.set_room(Room::Tavern),
            KeyCode::Char('3') => self.state.set_room(Room::Courier),
            KeyCode::Char('4') => self.state.set_room(Room::Market),
            KeyCode::Char('5') => self.state.set_room(Room::Keep),
            KeyCode::Char('6') => self.state.set_room(Room::Manor),

            // List navigation (vim keys & arrows)
            KeyCode::Char('j') | KeyCode::Down => {
                self.increment_selection();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.decrement_selection();
            }

            // Inspect selected item
            KeyCode::Enter => {
                self.inspect_current_item().await;
            }

            // Modals & Interventions
            KeyCode::Char('s') => {
                // Open steer dialog, prefilling UID if selected on Census or Keep
                let prefill_uid = self
                    .selected_uid()
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| "1001".into());
                self.state.open_modal(ModalState::Steer {
                    uid_input: prefill_uid,
                    directive_input: String::new(),
                    focus: SteerFocus::Directive,
                    submitting: false,
                    error_msg: None,
                });
            }
            KeyCode::Char('d') => {
                // Open decree review
                let decree_id = self
                    .selected_decree_id()
                    .unwrap_or_else(|| "d-1-0001".into());
                let detail = self.state.data.decree_details.get(&decree_id).cloned();
                self.state.open_modal(ModalState::DecreeReview {
                    decree_id: decree_id.clone(),
                    detail,
                    scroll: 0,
                });
                if let Some(client) = &self.client {
                    if let Ok(d) = client.fetch_decree_detail(&decree_id).await {
                        self.state
                            .data
                            .decree_details
                            .insert(decree_id.clone(), d.clone());
                        if let ModalState::DecreeReview { detail, .. } = &mut self.state.modal {
                            *detail = Some(d);
                        }
                    }
                }
            }
            KeyCode::Char('e') => {
                // Open escalation inbox
                self.state.open_modal(ModalState::EscalationInbox {
                    selected: 0,
                    notices: self.state.data.escalations.clone(),
                    resolving: false,
                    resolution_input: String::new(),
                    submitting: false,
                    status_msg: None,
                });
            }
            KeyCode::Char('m') => {
                // Open mail compose dialog
                self.state.open_modal(ModalState::MailCompose {
                    to_input: "1000".into(),
                    subject_input: String::new(),
                    body_input: String::new(),
                    focus: MailFocus::Subject,
                    submitting: false,
                    status_msg: None,
                });
            }
            KeyCode::Char('f') => {
                // Toggle ticker per-room filter
                self.state.ticker.filter_by_room = !self.state.ticker.filter_by_room;
                let mode = if self.state.ticker.filter_by_room {
                    format!("Filtered to {} only", self.state.current_room.short_name())
                } else {
                    "Showing all events".into()
                };
                self.state.set_status(mode, false);
            }
            KeyCode::Char('r') => {
                // Manual full state refresh
                if let Some(client) = &self.client {
                    match client.refresh_village_data().await {
                        Ok(data) => {
                            self.state.data = data;
                            self.state.set_status("Village state refreshed", false);
                        }
                        Err(e) => {
                            self.state.set_status(format!("Refresh error: {e}"), true);
                        }
                    }
                } else {
                    self.state
                        .set_status("Demo mode: fixture state refreshed", false);
                }
            }
            KeyCode::Char('?') => {
                self.state.open_modal(ModalState::Help);
            }
            _ => {}
        }
    }

    async fn handle_modal_key(&mut self, key: KeyEvent) {
        match &mut self.state.modal {
            ModalState::Help | ModalState::Detail { .. } | ModalState::DecreeReview { .. } => {
                if key.code == KeyCode::Esc
                    || key.code == KeyCode::Enter
                    || key.code == KeyCode::Char('q')
                {
                    self.state.close_modal();
                }
            }
            ModalState::Steer {
                uid_input,
                directive_input,
                focus,
                submitting,
                error_msg,
            } => {
                if key.code == KeyCode::Esc {
                    self.state.close_modal();
                    return;
                }
                if key.code == KeyCode::Tab {
                    *focus = match *focus {
                        SteerFocus::Uid => SteerFocus::Directive,
                        SteerFocus::Directive => SteerFocus::Submit,
                        SteerFocus::Submit => SteerFocus::Uid,
                    };
                    return;
                }
                if key.code == KeyCode::BackTab {
                    *focus = match *focus {
                        SteerFocus::Uid => SteerFocus::Submit,
                        SteerFocus::Directive => SteerFocus::Uid,
                        SteerFocus::Submit => SteerFocus::Directive,
                    };
                    return;
                }
                match focus {
                    SteerFocus::Uid => match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => uid_input.push(c),
                        KeyCode::Backspace => {
                            uid_input.pop();
                        }
                        KeyCode::Enter => *focus = SteerFocus::Directive,
                        _ => {}
                    },
                    SteerFocus::Directive => match key.code {
                        KeyCode::Char(c) => directive_input.push(c),
                        KeyCode::Backspace => {
                            directive_input.pop();
                        }
                        KeyCode::Enter => *focus = SteerFocus::Submit,
                        _ => {}
                    },
                    SteerFocus::Submit => {
                        if key.code == KeyCode::Enter {
                            let uid: u32 = uid_input.trim().parse().unwrap_or(0);
                            let dir = directive_input.trim();
                            if uid == 0 || dir.is_empty() {
                                *error_msg = Some("Invalid UID or empty directive".into());
                                return;
                            }
                            *submitting = true;
                            if let Some(client) = &self.client {
                                match client.steer_agent(uid, dir).await {
                                    Ok(res) => {
                                        self.state.set_status(
                                            format!("Directive injected into UID {uid} (correlation: {})", res.correlation),
                                            false,
                                        );
                                        self.state.close_modal();
                                    }
                                    Err(e) => {
                                        *submitting = false;
                                        *error_msg = Some(e);
                                    }
                                }
                            } else {
                                self.state.set_status(
                                    format!("Demo: Directive injected into UID {uid}"),
                                    false,
                                );
                                self.state.close_modal();
                            }
                        }
                    }
                }
            }
            ModalState::EscalationInbox {
                selected,
                notices,
                resolving,
                resolution_input,
                submitting,
                status_msg,
            } => {
                if *resolving {
                    match key.code {
                        KeyCode::Esc => {
                            *resolving = false;
                            resolution_input.clear();
                        }
                        KeyCode::Char(c) => {
                            resolution_input.push(c);
                        }
                        KeyCode::Backspace => {
                            resolution_input.pop();
                        }
                        KeyCode::Enter => {
                            let verdict = resolution_input.trim();
                            if verdict.is_empty() {
                                *status_msg = Some("Resolution text must not be empty".into());
                                return;
                            }
                            if let Some(notice) = notices.get(*selected) {
                                let notice_id = notice.id.clone();
                                *submitting = true;
                                if let Some(client) = &self.client {
                                    match client.resolve_escalation(&notice_id, verdict).await {
                                        Ok(_) => {
                                            self.state.set_status(
                                                format!("Escalation '{notice_id}' resolved"),
                                                false,
                                            );
                                            // Refresh notices
                                            if let Ok(new_notices) = client
                                                .get::<Vec<EscalationView>>(ENDPOINT_ESCALATIONS)
                                                .await
                                            {
                                                self.state.data.escalations = new_notices;
                                            }
                                            self.state.close_modal();
                                        }
                                        Err(e) => {
                                            *submitting = false;
                                            *status_msg = Some(e);
                                        }
                                    }
                                } else {
                                    self.state.set_status(
                                        format!("Demo: Escalation '{notice_id}' resolved"),
                                        false,
                                    );
                                    self.state.close_modal();
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => self.state.close_modal(),
                        KeyCode::Char('j') | KeyCode::Down if !notices.is_empty() => {
                            *selected = (*selected + 1).min(notices.len() - 1);
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            *selected = selected.saturating_sub(1);
                        }
                        KeyCode::Char('r') | KeyCode::Enter => {
                            if let Some(esc) = notices.get(*selected) {
                                if !esc.resolved {
                                    *resolving = true;
                                } else {
                                    *status_msg = Some("Notice is already resolved".into());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            ModalState::MailCompose {
                to_input,
                subject_input,
                body_input,
                focus,
                submitting,
                status_msg,
            } => {
                if key.code == KeyCode::Esc {
                    self.state.close_modal();
                    return;
                }
                if key.code == KeyCode::Tab {
                    *focus = match *focus {
                        MailFocus::To => MailFocus::Subject,
                        MailFocus::Subject => MailFocus::Body,
                        MailFocus::Body => MailFocus::Submit,
                        MailFocus::Submit => MailFocus::To,
                    };
                    return;
                }
                if key.code == KeyCode::BackTab {
                    *focus = match *focus {
                        MailFocus::To => MailFocus::Submit,
                        MailFocus::Subject => MailFocus::To,
                        MailFocus::Body => MailFocus::Subject,
                        MailFocus::Submit => MailFocus::Body,
                    };
                    return;
                }
                match focus {
                    MailFocus::To => match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => to_input.push(c),
                        KeyCode::Backspace => {
                            to_input.pop();
                        }
                        KeyCode::Enter => *focus = MailFocus::Subject,
                        _ => {}
                    },
                    MailFocus::Subject => match key.code {
                        KeyCode::Char(c) => subject_input.push(c),
                        KeyCode::Backspace => {
                            subject_input.pop();
                        }
                        KeyCode::Enter => *focus = MailFocus::Body,
                        _ => {}
                    },
                    MailFocus::Body => match key.code {
                        KeyCode::Char(c) => body_input.push(c),
                        KeyCode::Backspace => {
                            body_input.pop();
                        }
                        KeyCode::Enter => *focus = MailFocus::Submit,
                        _ => {}
                    },
                    MailFocus::Submit => {
                        if key.code == KeyCode::Enter {
                            let to: u32 = to_input.trim().parse().unwrap_or(0);
                            let subj = subject_input.trim();
                            let body = body_input.trim();
                            if to == 0 || subj.is_empty() || body.is_empty() {
                                *status_msg = Some("All fields required".into());
                                return;
                            }
                            *submitting = true;
                            if let Some(client) = &self.client {
                                match client.send_mail(to, Some("operator"), subj, body).await {
                                    Ok(_) => {
                                        self.state
                                            .set_status(format!("Mail spooled to UID {to}"), false);
                                        self.state.close_modal();
                                    }
                                    Err(e) => {
                                        *submitting = false;
                                        *status_msg = Some(e);
                                    }
                                }
                            } else {
                                self.state
                                    .set_status(format!("Demo: Mail spooled to UID {to}"), false);
                                self.state.close_modal();
                            }
                        }
                    }
                }
            }
            ModalState::None => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            // Click on room tab row (y = 1 or y = 2)
            if mouse.row == 1 || mouse.row == 2 {
                let col = mouse.column;
                // Roughly 6 rooms distributed evenly
                let room_idx = (col as usize) / 16;
                if let Some(room) = Room::from_index(room_idx) {
                    self.state.set_room(room);
                }
            }
        }
    }

    fn increment_selection(&mut self) {
        let max_items = self.current_room_item_count();
        if max_items > 0 {
            let sel = self.state.current_sel_mut();
            sel.list_index = (sel.list_index + 1).min(max_items - 1);
        }
    }

    fn decrement_selection(&mut self) {
        let sel = self.state.current_sel_mut();
        sel.list_index = sel.list_index.saturating_sub(1);
    }

    fn current_room_item_count(&self) -> usize {
        match self.state.current_room {
            Room::Guild => self.state.data.commissions.len(),
            Room::Tavern => self.state.data.landmarks.tavern.patrons.len(),
            Room::Courier => self.state.data.landmarks.courier.spools.len(),
            Room::Market => self.state.data.landmarks.market.skills.len(),
            Room::Keep => self.state.data.census.len(),
            Room::Manor => self.state.data.decrees.len(),
        }
    }

    fn selected_uid(&self) -> Option<u32> {
        match self.state.current_room {
            Room::Keep => {
                let idx = self.state.current_sel().list_index;
                self.state.data.census.get(idx).map(|c| c.uid)
            }
            Room::Courier => {
                let idx = self.state.current_sel().list_index;
                self.state
                    .data
                    .landmarks
                    .courier
                    .spools
                    .get(idx)
                    .map(|s| s.uid)
            }
            Room::Guild => {
                let idx = self.state.current_sel().list_index;
                self.state
                    .data
                    .commissions
                    .get(idx)
                    .and_then(|c| c.roster.first().copied())
            }
            _ => None,
        }
    }

    fn selected_decree_id(&self) -> Option<String> {
        let idx = self.state.current_sel().list_index;
        self.state
            .data
            .decrees
            .get(idx)
            .map(|d| d.decree_id.clone())
    }

    async fn inspect_current_item(&mut self) {
        let idx = self.state.current_sel().list_index;
        match self.state.current_room {
            Room::Guild => {
                if let Some(comm) = self.state.data.commissions.get(idx) {
                    let slug = comm.slug.to_string();
                    let detail = if let Some(d) = self.state.data.commission_details.get(&slug) {
                        d.clone()
                    } else if let Some(client) = &self.client {
                        if let Ok(d) = client.fetch_commission_detail(&slug).await {
                            self.state
                                .data
                                .commission_details
                                .insert(slug.clone(), d.clone());
                            d
                        } else {
                            return;
                        }
                    } else {
                        return;
                    };

                    let content = format!(
                        "Commission: {}\nState: {}\nRoster: {:?}\nGit Head: {:?}\n\nBrief:\n{}\n\nTasks:\n{}",
                        detail.summary.slug,
                        detail.summary.state,
                        detail.summary.roster,
                        detail.git_head,
                        detail.brief.as_deref().unwrap_or("(none)"),
                        detail
                            .indenture_tasks
                            .iter()
                            .map(|t| format!(" - [{}] UID {}: {}", t.status, t.uid, t.task))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );

                    self.state.open_modal(ModalState::Detail {
                        title: format!("Commission: {}", detail.summary.slug),
                        content,
                        scroll: 0,
                    });
                }
            }
            Room::Manor => {
                if let Some(d) = self.state.data.decrees.get(idx) {
                    let id = d.decree_id.clone();
                    let detail = self.state.data.decree_details.get(&id).cloned();
                    self.state.open_modal(ModalState::DecreeReview {
                        decree_id: id,
                        detail,
                        scroll: 0,
                    });
                }
            }
            Room::Keep => {
                if let Some(c) = self.state.data.census.get(idx) {
                    let content = format!(
                        "Parcel Lease: parcel{}/\nIdentity: {}\nState: {}\nCommission: {:?}\nSessions Run: {}\n\nTelemetry Counters:\n - Input Tokens:  {}\n - Output Tokens: {}\n - Cost:          ${:.4}\n - Traces Logged: {}\n\nLast Event: {:?} ({:?})\nTITLE.md Content Digest: {}",
                        c.uid,
                        c.identity,
                        c.state,
                        c.commission_slug,
                        c.session_count,
                        c.counters.input_tokens,
                        c.counters.output_tokens,
                        (c.counters.cost_micro_usd as f64) / 1_000_000.0,
                        c.counters.traces,
                        c.last_event,
                        c.last_event_ts,
                        c.title_digest,
                    );
                    self.state.open_modal(ModalState::Detail {
                        title: format!("Census Lease: UID {}", c.uid),
                        content,
                        scroll: 0,
                    });
                }
            }
            Room::Tavern => {
                if let Some(p) = self.state.data.landmarks.tavern.patrons.get(idx) {
                    let content = format!(
                        "Patron: {}\nLocation: tavern/patrons/PROPRIETOR{}/\nHome Backup: home.{}.bak/\n\nMutated agent definition is preserved in stasis until awakened for a new commission lease.",
                        p, p, p
                    );
                    self.state.open_modal(ModalState::Detail {
                        title: format!("Patron: {p}"),
                        content,
                        scroll: 0,
                    });
                }
            }
            Room::Courier => {
                if let Some(s) = self.state.data.landmarks.courier.spools.get(idx) {
                    let content = format!(
                        "Mail Spool: courier/mail/{}/\nRecipient: UID {}\nUnread Count: {}\nDelivered/Read Count: {}\n\nMail messages are sequentially piped to agent stdin at session task boundaries (SPEC §6).",
                        s.uid, s.uid, s.unread, s.read
                    );
                    self.state.open_modal(ModalState::Detail {
                        title: format!("Mail Spool: UID {}", s.uid),
                        content,
                        scroll: 0,
                    });
                }
            }
            Room::Market => {
                if let Some(sk) = self.state.data.landmarks.market.skills.get(idx) {
                    let content = format!(
                        "Skill Capability: {}\nPath: market/skills/{}/SKILL.md\n\nModular capability package declaratively mounted into parcel containers based on blueprint declarations.",
                        sk, sk
                    );
                    self.state.open_modal(ModalState::Detail {
                        title: format!("Skill: {sk}"),
                        content,
                        scroll: 0,
                    });
                }
            }
        }
    }
}
