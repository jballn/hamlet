//! FakeAgent — the scripted pi mock (ADR-0015 cl. 5; contract: the
//! `hamlet-core::session` module rustdoc "FakeAgent contract").
//!
//! Stands in for pi in every non-guarded supervision/delivery test
//! (CONVENTIONS §9). It speaks exactly the pi RPC JSONL dialect the daemon
//! emits through [`hamlet_core::session::pi_binding`] — one wire, one
//! reader ([`hamlet_core::session::parse_pi_command`]), so fake and real pi
//! cannot drift on the accepted command set.
//!
//! # Invocation
//!
//! ```text
//! fakeagent [--script <path>]        (script path also via FAKEAGENT_SCRIPT)
//! ```
//!
//! Unknown argv tokens are ignored, so the daemon's real session argv
//! (`pi --mode rpc [--provider P] [--model M] [--continue]`) can be passed
//! through verbatim by the host-process test spawner.
//!
//! # Script schema (JSON; owned by Sprint 03 step 02, metric fields by step 04)
//!
//! ```json
//! {
//!   "log": "/tmp/fakeagent.log",     // append every received stdin line
//!   "event_delay_ms": 5,             // deliberate delay before each record
//!   "never_ready": false,            // suppress get_state responses (readiness timeout)
//!   "slow_task": false,              // hold agent_settled until the "settle" cue
//!   "ignore_eof": false,             // do not exit on stdin EOF (grace-window tests)
//!   "crash_exit_code": 1,            // exit code for scripted crashes
//!   "crash_after_commands": null,    // crash after N pi commands (cues excluded)
//!   "crash_after_frames": null,      // crash mid-run after N emitted records
//!   "crash_if_argv_lacks": null,     // disarm BOTH crash rules when this argv
//!                                    // token is present (QA step 05: the daemon
//!                                    // passes `--continue` to incarnations > 0,
//!                                    // so `"--continue"` = crash only the first
//!                                    // life, then let the auto-restart survive)
//!   "malformed": ["not json {{{"],   // raw lines emitted on the "malformed" cue
//!   "usage": { "input": 10, "output": 5, "totalTokens": 15 },
//!   "expect_env": { "ANTHROPIC_API_KEY": "test-key-do-not-use" },
//!
//!   // --- step 04: configurable metric streams (all additive/defaulted) ---
//!   "usage_sequence": [ { "input": 10, "output": 2, "totalTokens": 12 },
//!                       { "input": 10, "output": 5, "totalTokens": 15 } ],
//!   "turns_per_run": 2,              // turn blocks emitted per run
//!   "emit_message_end": true,        // authoritative message_end{message.usage}
//!   "tool_calls": [ { "name": "bash", "delay_ms": 40, "is_error": false } ],
//!   "flood_frames": 10000,           // soak: cumulative message_update count
//!   "flood_usage": { "input": 1, "output": 1, "totalTokens": 2,
//!                    "cost": { "total": 0.000001 } },
//!   "malformed_every_n": 250,        // interleave garbage every N records
//!   "unknown_records": ["hamlet_probe"] // unmodelled record types (drift)
//! }
//! ```
//!
//! Metric-stream semantics (deterministic, golden-testable):
//!
//! - `usage_sequence` replaces the single `usage` snapshot when non-empty;
//!   each entry is emitted as one `message_update` and is **cumulative** for
//!   the assistant message, exactly like pi (docs `json.md`), so a consumer
//!   must difference snapshots rather than sum them.
//! - `flood_frames` + `flood_usage` supersede both: the turn emits exactly
//!   `flood_frames` `message_update` records whose cumulative usage is
//!   `flood_usage × i` for `i = 1…N`. Final totals are therefore
//!   `flood_usage × N` per turn — the soak-test golden.
//! - `emit_message_end` appends `{"type":"message_end","message":{"role":
//!   "assistant","usage":<last snapshot>}}`, pi's authoritative final usage.
//! - `tool_calls` emits `tool_execution_start` → (delay) →
//!   `tool_execution_end` per entry, with a stable `toolCallId` per call.
//! - `malformed_every_n` interleaves garbage from the `malformed` list
//!   (cycling) after every Nth emitted record — script-driven because the
//!   daemon owns stdin, so a test cannot cue mid-run (same reason
//!   `settle_after_ms` exists).
//!
//! `expect_env` is checked once at startup; the result is written to the
//! script `log` as `env-check ok` / `env-check mismatch:<KEY>` /
//! `env-check missing:<KEY>` — values are never logged (CONVENTIONS §6).
//!
//! # Cues (test-only stdin records, never echoed)
//!
//! `{"type":"fake_cue","cue":"<name>"}`:
//! - `crash` — exit immediately with `crash_exit_code` (no goodbye);
//! - `hang` — freeze: keep reading, respond to nothing;
//! - `settle` — release a held `slow_task` run (emits the settle sequence);
//! - `malformed` — emit the next scripted malformed stdout line (cycling).
//!
//! Held runs (`slow_task`/`settle_after_ms`) never block stdin: a reader
//! thread feeds the command loop, so interrupts (steer/abort) and cues are
//! processed while a task is in flight — exactly like real pi.
//!
//! # Default (unscripted) behavior
//!
//! Minimal faithful pi emulation per the contract: `prompt` →
//! `response{disposition:"started"}` + `agent_start`, `turn_start`, one
//! `message_update` (scripted usage), `turn_end`, `agent_end{willRetry:false}`,
//! `agent_settled`; `steer` → `response{disposition:"queued"}` (delivered as
//! continuation runs after the current run); `abort` → end the run, continue
//! queued steering, respond only once settled; `clear_queue` → response with
//! the dropped texts; `get_state` → response with `isStreaming`. Stdin EOF →
//! clean exit 0 (unless `ignore_eof`). Stderr is diagnostics only, never
//! protocol. Deterministic: record content/order derive solely from the
//! script and the received command sequence.

use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufWriter, Write};
use std::path::PathBuf;
use std::process::exit;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use hamlet_core::session::{parse_pi_command, Cost, PiCommand, Usage};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    #[serde(default)]
    log: Option<PathBuf>,
    #[serde(default)]
    event_delay_ms: u64,
    #[serde(default)]
    never_ready: bool,
    #[serde(default)]
    slow_task: bool,
    #[serde(default)]
    settle_after_ms: Option<u64>,
    #[serde(default)]
    ignore_eof: bool,
    #[serde(default = "default_crash_exit_code")]
    crash_exit_code: i32,
    #[serde(default)]
    crash_after_commands: Option<u32>,
    #[serde(default)]
    crash_after_frames: Option<u32>,
    #[serde(default)]
    crash_if_argv_lacks: Option<String>,
    #[serde(default)]
    malformed: Vec<String>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    expect_env: BTreeMap<String, String>,
    // --- step 04: configurable metric streams ---
    #[serde(default)]
    usage_sequence: Vec<Usage>,
    #[serde(default = "default_turns_per_run")]
    turns_per_run: u32,
    #[serde(default)]
    emit_message_end: bool,
    #[serde(default)]
    tool_calls: Vec<ToolCallScript>,
    #[serde(default)]
    flood_frames: u32,
    #[serde(default)]
    flood_usage: Option<Usage>,
    #[serde(default)]
    malformed_every_n: u32,
    #[serde(default)]
    unknown_records: Vec<String>,
}

/// One scripted tool execution inside a turn.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallScript {
    name: String,
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    is_error: bool,
}

fn default_crash_exit_code() -> i32 {
    1
}

fn default_turns_per_run() -> u32 {
    1
}

/// Scale a usage snapshot by `factor` — the cumulative growth per flood
/// frame (`flood_usage × i`), so a soak's expected totals are exact integer
/// multiples of the step.
fn scale_usage(step: Usage, factor: u64) -> Usage {
    let scale = |value: u64| value.saturating_mul(factor);
    let cost = |value: f64| {
        if value.is_finite() {
            value * factor as f64
        } else {
            value
        }
    };
    Usage {
        input: scale(step.input),
        output: scale(step.output),
        cache_read: scale(step.cache_read),
        cache_write: scale(step.cache_write),
        total_tokens: scale(step.total_tokens),
        cost: Cost {
            input: cost(step.cost.input),
            output: cost(step.cost.output),
            cache_read: cost(step.cost.cache_read),
            cache_write: cost(step.cost.cache_write),
            total: cost(step.cost.total),
        },
    }
}

impl Default for Script {
    fn default() -> Self {
        Self {
            log: None,
            event_delay_ms: 0,
            never_ready: false,
            slow_task: false,
            settle_after_ms: None,
            ignore_eof: false,
            crash_exit_code: default_crash_exit_code(),
            crash_after_commands: None,
            crash_after_frames: None,
            crash_if_argv_lacks: None,
            malformed: Vec::new(),
            usage: None,
            expect_env: BTreeMap::new(),
            usage_sequence: Vec::new(),
            turns_per_run: default_turns_per_run(),
            emit_message_end: false,
            tool_calls: Vec::new(),
            flood_frames: 0,
            flood_usage: None,
            malformed_every_n: 0,
            unknown_records: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename = "fake_cue")]
struct Cue {
    cue: String,
}

struct State {
    script: Script,
    log: Option<io::BufWriter<std::fs::File>>,
    out: BufWriter<io::Stdout>,
    streaming: bool,
    /// `slow_task`: a run started but `agent_end`/`agent_settled` are held
    /// back until the `settle` cue.
    holding_settle: bool,
    /// `settle_after_ms`: auto-release deadline for a held run (the cue
    /// channel is not the only release path — the daemon owns stdin, so
    /// timed tests cannot send cues).
    hold_deadline: Option<Instant>,
    /// `hang` cue: respond to nothing, keep reading.
    frozen: bool,
    /// Are the scripted crash rules armed? (`crash_if_argv_lacks` disarms
    /// them when a token — e.g. `--continue` on restarted incarnations —
    /// IS present in argv.)
    crash_armed: bool,
    /// Queued steering/follow-up texts, drained as continuation runs.
    steer_queue: VecDeque<String>,
    /// `abort` ids whose responses are due once the session settles.
    pending_aborts: Vec<String>,
    commands_seen: u32,
    malformed_next: usize,
    /// Records emitted so far (drives `malformed_every_n` interleaving).
    emitted: u32,
    /// Monotonic tool-call id allocator (stable per call, deterministic).
    tool_seq: u32,
}

impl State {
    fn emit_value(&mut self, value: Value) {
        self.emit_line(&value.to_string());
    }

    fn emit_line(&mut self, line: &str) {
        if self.script.event_delay_ms > 0 {
            sleep(Duration::from_millis(self.script.event_delay_ms));
        }
        // Stdout is the protocol channel: one record per LF, flushed
        // immediately so supervisor reads never wait on buffering.
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
        self.emitted += 1;
        // Die mid-run (step 04): an agent killed between frames, so the
        // daemon's telemetry has open spans to close.
        if self.crash_armed {
            if let Some(limit) = self.script.crash_after_frames {
                if self.emitted >= limit {
                    self.crash(&format!("after {} frames", self.emitted));
                }
            }
        }
        // Scripted garbage interleaving (step 04): the daemon owns stdin, so
        // a resilience test cannot cue mid-run — the script does it instead.
        let every = self.script.malformed_every_n;
        if every > 0 && self.emitted.is_multiple_of(every) {
            self.emit_malformed_line();
        }
    }

    /// Emit the next scripted malformed line (cycling), bypassing the
    /// interleave counter so garbage cannot cascade.
    fn emit_malformed_line(&mut self) {
        let list = &self.script.malformed;
        let line = if list.is_empty() {
            "###not-json###".to_owned()
        } else {
            list[self.malformed_next % list.len()].clone()
        };
        self.malformed_next += 1;
        if self.script.event_delay_ms > 0 {
            sleep(Duration::from_millis(self.script.event_delay_ms));
        }
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }

    fn log_line(&mut self, line: &str) {
        if let Some(log) = self.log.as_mut() {
            let _ = writeln!(log, "{line}");
            let _ = log.flush();
        }
    }

    fn response(&mut self, id: Option<&str>, command: &str, success: bool, data: Option<Value>) {
        let mut record = json!({ "type": "response", "command": command, "success": success });
        if let Some(id) = id {
            record["id"] = json!(id);
        }
        if let Some(data) = data {
            record["data"] = data;
        }
        self.emit_value(record);
    }

    fn parse_failure(&mut self, error: &str) {
        // pi-style record-level parse failure: no id (it could not be read).
        self.response(None, "parse", false, None);
        let _ = writeln!(io::stderr(), "fakeagent: parse failure: {error}");
    }

    fn usage(&self) -> Usage {
        self.script.usage.unwrap_or_else(|| Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..Usage::default()
        })
    }

    /// The cumulative usage snapshots this turn streams, in emission order
    /// (pi semantics: each `message_update.usage` is cumulative for the
    /// in-flight assistant response). `flood_frames` supersedes the
    /// sequence so a soak has an exactly computable golden total.
    fn usage_snapshots(&self) -> Vec<Usage> {
        if self.script.flood_frames > 0 {
            let step = self.script.flood_usage.unwrap_or_else(|| Usage {
                input: 1,
                output: 1,
                total_tokens: 2,
                ..Usage::default()
            });
            return (1..=self.script.flood_frames)
                .map(|i| scale_usage(step, u64::from(i)))
                .collect();
        }
        if !self.script.usage_sequence.is_empty() {
            return self.script.usage_sequence.clone();
        }
        vec![self.usage()]
    }

    fn run_turn(&mut self) {
        self.emit_value(json!({ "type": "turn_start" }));
        let snapshots = self.usage_snapshots();
        for snapshot in &snapshots {
            let usage: Value = serde_json::to_value(snapshot).expect("usage serializes");
            self.emit_value(json!({ "type": "message_update", "usage": usage }));
        }
        if self.script.emit_message_end {
            // pi's authoritative final message (docs `json.md`): the last
            // cumulative snapshot, wrapped in an assistant message.
            let last = snapshots.last().copied().unwrap_or_else(|| self.usage());
            let usage: Value = serde_json::to_value(last).expect("usage serializes");
            self.emit_value(json!({
                "type": "message_end",
                "message": { "role": "assistant", "content": [], "usage": usage },
            }));
        }
        // Clone before emitting: `emit_value` needs `&mut self` (the script
        // is immutable data, so the clone is a read-only snapshot).
        for tool in self.script.tool_calls.clone() {
            self.tool_seq += 1;
            let call_id = format!("call_{}", self.tool_seq);
            let name = tool.name.clone();
            self.emit_value(json!({
                "type": "tool_execution_start",
                "toolCallId": call_id,
                "toolName": name,
                "args": {},
            }));
            if tool.delay_ms > 0 {
                sleep(Duration::from_millis(tool.delay_ms));
            }
            self.emit_value(json!({
                "type": "tool_execution_end",
                "toolCallId": call_id,
                "toolName": name,
                "isError": tool.is_error,
                "result": { "content": [], "details": {} },
            }));
        }
        for record_type in self.script.unknown_records.clone() {
            self.emit_value(json!({ "type": record_type }));
        }
        self.emit_value(json!({ "type": "turn_end" }));
    }

    fn start_run(&mut self) {
        self.streaming = true;
        self.emit_value(json!({ "type": "agent_start" }));
        for _ in 0..self.script.turns_per_run.max(1) {
            self.run_turn();
        }
        if self.script.slow_task || self.script.settle_after_ms.is_some() {
            self.holding_settle = true;
            self.hold_deadline = self
                .script
                .settle_after_ms
                .map(|ms| Instant::now() + Duration::from_millis(ms));
        } else {
            self.finish_run();
        }
    }

    /// End the current run: `agent_end`, continuation runs for queued
    /// steering (pi continues queued messages — ADR-0015 cl. 3), then the
    /// task boundary `agent_settled`, then any due `abort` responses.
    fn finish_run(&mut self) {
        self.holding_settle = false;
        self.hold_deadline = None;
        self.emit_value(json!({ "type": "agent_end", "willRetry": false }));
        while !self.steer_queue.is_empty() {
            self.emit_value(json!({ "type": "agent_start" }));
            for _ in 0..self.script.turns_per_run.max(1) {
                self.run_turn();
            }
            self.emit_value(json!({ "type": "agent_end", "willRetry": false }));
            self.steer_queue.pop_front();
        }
        self.streaming = false;
        self.emit_value(json!({ "type": "agent_settled" }));
        for id in std::mem::take(&mut self.pending_aborts) {
            self.response(Some(&id), "abort", true, None);
        }
    }

    fn crash(&mut self, context: &str) {
        let _ = writeln!(io::stderr(), "fakeagent: crashing ({context})");
        let _ = self.out.flush();
        if let Some(log) = self.log.as_mut() {
            let _ = log.flush();
        }
        exit(self.script.crash_exit_code);
    }

    fn handle_command(&mut self, command: PiCommand) {
        self.commands_seen += 1;
        if self.crash_armed {
            if let Some(limit) = self.script.crash_after_commands {
                if self.commands_seen >= limit {
                    self.crash(&format!("after {} commands", self.commands_seen));
                }
            }
        }
        if self.frozen {
            return; // hang cue: read and log, answer nothing
        }
        match command {
            PiCommand::GetState { id } => {
                if self.script.never_ready {
                    return; // readiness probe goes unanswered (timeout test)
                }
                self.response(
                    Some(&id),
                    "get_state",
                    true,
                    Some(json!({ "isStreaming": self.streaming })),
                );
            }
            PiCommand::Prompt { id, .. } => {
                if self.streaming {
                    // pi rejects mid-stream prompts (no streamingBehavior);
                    // hamlet never sends one by construction.
                    self.emit_value(json!({
                        "type": "response", "id": id, "command": "prompt",
                        "success": false, "error": "agent is streaming",
                    }));
                    return;
                }
                self.response(
                    Some(&id),
                    "prompt",
                    true,
                    Some(json!({ "disposition": "started" })),
                );
                self.start_run();
            }
            PiCommand::Steer { id, message } => {
                self.response(
                    Some(&id),
                    "steer",
                    true,
                    Some(json!({ "disposition": "queued" })),
                );
                self.steer_queue.push_back(message);
            }
            PiCommand::Abort { id } => {
                if self.streaming || self.holding_settle {
                    // Interrupt the run; respond only once settled.
                    self.pending_aborts.push(id);
                    self.finish_run();
                } else {
                    self.response(Some(&id), "abort", true, None);
                }
            }
            PiCommand::ClearQueue { id } => {
                let dropped: Vec<String> = self.steer_queue.drain(..).collect();
                self.response(
                    Some(&id),
                    "clear_queue",
                    true,
                    Some(json!({ "dropped": dropped })),
                );
            }
        }
    }

    fn handle_cue(&mut self, cue: &str) {
        match cue {
            "crash" => self.crash("crash cue"),
            "hang" => self.frozen = true,
            "settle" => {
                if self.holding_settle {
                    self.finish_run();
                }
            }
            "malformed" => self.emit_malformed_line(),
            other => {
                let _ = writeln!(io::stderr(), "fakeagent: unknown cue {other:?}");
            }
        }
    }
}

fn load_script() -> Script {
    let mut path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--script" {
            path = args.next().map(PathBuf::from);
        }
        // Everything else (pi argv tokens) is ignored by design.
    }
    if path.is_none() {
        path = std::env::var("FAKEAGENT_SCRIPT").ok().map(PathBuf::from);
    }
    match path {
        None => Script::default(),
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("fakeagent: cannot read script {}: {e}", path.display())
            });
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("fakeagent: bad script {}: {e}", path.display()))
        }
    }
}

fn open_log(script: &Script) -> Option<io::BufWriter<std::fs::File>> {
    script.log.as_ref().map(|path| {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|e| panic!("fakeagent: cannot open log {}: {e}", path.display()));
        io::BufWriter::new(file)
    })
}

fn check_env(state: &mut State) {
    // Clone first: iterating the script while logging would double-borrow.
    let expected = state.script.expect_env.clone();
    for (key, want) in &expected {
        let verdict = match std::env::var(key) {
            Ok(actual) if actual == *want => "ok",
            Ok(_) => "mismatch",
            Err(std::env::VarError::NotPresent) => "missing",
            Err(std::env::VarError::NotUnicode(_)) => "mismatch",
        };
        // Never log values (CONVENTIONS §6) — the key name only.
        state.log_line(&format!("env-check {verdict}:{key}"));
    }
}

fn main() {
    let script = load_script();
    // QA step 05: crash rules can be scoped to lives whose argv LACKS a
    // token — restarted incarnations carry `--continue` (ADR-0015 cl. 4),
    // so a script can crash only the first life and let the auto-restart
    // survive to drain the queue.
    let argv: Vec<String> = std::env::args().collect();
    let crash_armed = script
        .crash_if_argv_lacks
        .as_ref()
        .is_none_or(|token| !argv.iter().any(|arg| arg == token));
    let mut state = State {
        log: open_log(&script),
        script,
        out: BufWriter::new(io::stdout()),
        streaming: false,
        holding_settle: false,
        hold_deadline: None,
        frozen: false,
        crash_armed,
        steer_queue: VecDeque::new(),
        pending_aborts: Vec::new(),
        commands_seen: 0,
        malformed_next: 0,
        emitted: 0,
        tool_seq: 0,
    };
    check_env(&mut state);
    state.log_line("# fakeagent start");

    // Stdin reader thread: a held run (`slow_task`/`settle_after_ms`) must
    // never block command processing — interrupts arrive mid-task, like
    // real pi. `None` on the channel = EOF.
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(Some(line)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });

    loop {
        // Timed hold expired → release the settle sequence.
        if state.holding_settle {
            if let Some(deadline) = state.hold_deadline {
                if Instant::now() >= deadline {
                    state.finish_run();
                }
            }
        }
        let msg = match state.hold_deadline.filter(|_| state.holding_settle) {
            Some(deadline) => {
                let wait = deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1));
                match rx.recv_timeout(wait) {
                    Ok(msg) => msg,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => None,
                }
            }
            None => rx.recv().unwrap_or(None),
        };
        let Some(line) = msg else { break }; // stdin EOF
        if line.trim().is_empty() {
            continue;
        }
        state.log_line(&line);
        // Cues are test-only vocabulary, intercepted before the pi dialect
        // and never echoed (contract).
        if let Ok(parsed) = serde_json::from_str::<Value>(&line) {
            if parsed.get("type").and_then(Value::as_str) == Some("fake_cue") {
                if let Ok(cue) = serde_json::from_value::<Cue>(parsed) {
                    state.handle_cue(&cue.cue);
                } else {
                    state.parse_failure("malformed fake_cue record");
                }
                continue;
            }
        }
        match parse_pi_command(&line) {
            Ok(command) => state.handle_command(command),
            Err(e) => state.parse_failure(&e.to_string()),
        }
    }

    // Stdin EOF: pi exits cleanly; `ignore_eof` keeps the process alive so
    // the supervisor's grace window has to hard-kill it.
    if state.script.ignore_eof {
        let _ = writeln!(io::stderr(), "fakeagent: ignoring EOF (scripted)");
        let _ = state.out.flush();
        loop {
            sleep(Duration::from_millis(50));
        }
    }
    let _ = state.out.flush();
    if let Some(log) = state.log.as_mut() {
        let _ = log.flush();
    }
    exit(0);
}
