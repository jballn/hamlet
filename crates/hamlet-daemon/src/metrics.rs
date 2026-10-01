//! Daemon-parsed session telemetry (SPEC §6 "Agents do not self-report
//! metrics", §11.2; extraction semantics ADR-0016, sink architecture and
//! query surface ADR-0017).
//!
//! Folds a session's pi JSON-RPC stdout stream into sled records through
//! the Sprint-01 telemetry API ([`crate::telemetry`]), namespaced by
//! commission/uid/session. Nothing here is orchestration state and nothing
//! here touches `charter.json` — the stream is *evidence*, the charter
//! stays the only truth (CONVENTIONS §2). The agent is never asked what it
//! consumed; every number is parsed off its output (SPEC §6).
//!
//! # Shape: pure fold inline, sled I/O in a task
//!
//! The invariant that decides the architecture is **telemetry may never
//! starve the delivery writer** (SPEC §6 sequential delivery; the step-03
//! single-writer funnel). So the "parser task per session" is split:
//!
//! - [`StreamExtractor`] is a **pure fold**: `(&AgentEvent, at_unix_ms)` →
//!   [`Observation`]s. No I/O, no clock reads, no allocation on the common
//!   path (ADR-0017 cl. 1). It runs inline in the life task's stdout arm — right where
//!   [`parse_agent_event`] already ran — so extraction adds arithmetic to
//!   the hot path and can never block a drain or an interrupt injection.
//! - [`SessionMetrics`] is the per-session **sink handle** the life task
//!   holds: a bounded observation channel plus a tokio task owning every
//!   sled write for that session ([`spawn`]). Feeding it is non-blocking
//!   ([`SessionMetrics::observe`] uses `try_send` and coalesces locally
//!   when the sink falls behind), so a stalled sled degrades telemetry,
//!   never supervision (ADR-0005 cl. 6).
//!
//! The sink batches: one wake folds every queued observation into a single
//! `add_tokens` + `add_cost_micro_usd` pair plus the append-only records,
//! so a 10k-frame stream collapses into a handful of sled writes. Under
//! backpressure additive observations coalesce losslessly while append-only
//! records are bounded and their loss counted (ADR-0017 cl. 2–3).
//!
//! # What is extracted (ADR-0016)
//!
//! - **Tokens** (ADR-0016 cl. 2). pi's `message_update.usage` is a
//!   *cumulative* snapshot for
//!   the in-flight assistant response (pi docs `json.md`), so the extractor
//!   keeps a baseline and emits only the **delta** — naive summing would
//!   multiply-count every streamed frame. `message_end.message.usage` is
//!   authoritative and closes the message. A snapshot *lower* than the
//!   baseline means the provider started a new message without a boundary
//!   frame: the whole snapshot is taken as the delta and the baseline
//!   rebased, so totals can never go negative or stall. Only `input` /
//!   `output` reach `tokens.v1` — the Sprint-01 [`TokenTally`] shape is
//!   fixed and ADR-0005 cl. 5 forbids reshaping a `.v1` record; cache,
//!   reasoning and `totalTokens` are counted in [`StreamStats`] and
//!   reported in the per-life summary trace instead.
//! - **Cost** (ADR-0016 cl. 3). `usage.cost` USD floats become integer
//!   micro-USD
//!   ([`cost_micro_usd`], round-half-away-from-zero, non-finite → 0).
//!   Cumulative snapshots are differenced *after* conversion, so no float
//!   drift accumulates across a stream.
//! - **Latency spikes** (ADR-0016 cl. 1). A measured span whose duration is
//!   `>= sessions.latency_spike_ms` (config v4) is appended to `latency.v1`.
//!   Operations: `agent-run` (`agent_start`→`agent_end`), `agent-task`
//!   (first `agent_start` of a task→`agent_settled`, spanning continuation
//!   runs), `agent-turn` (`turn_start`→`turn_end`), `tool-call`
//!   (`tool_execution_start`→`_end`, matched by `toolCallId`),
//!   `stream-gap` (consecutive-frame gap **while a task is in flight** —
//!   idle time never counts), and `delivery-ack` / `interrupt-ack` (a
//!   written wire id → its `response`). [`StreamExtractor::finish`] closes
//!   and evaluates every open span, so a session killed mid-task still
//!   records the spike that killed it.
//! - **Traces.** Stream-derived events only: interrupt/delivery
//!   acknowledgments (the `<corr>:{q,s,a,p}` response ids are the join key
//!   back to step 03's injection traces), tool-call markers, malformed
//!   frames, unknown record types (the pi-version-drift signal), and one
//!   `session-stream-summary` rollup per life. Process-lifecycle traces
//!   (`session-started`/`-stopped`/`-crashed`/…) stay in
//!   [`crate::supervise`]: one source per event, no duplicates.
//!
//! # Hostile-stream discipline (CONVENTIONS §6, §9)
//!
//! The stream is adversarial input. Malformed frames are counted and
//! skipped, never fatal — and their **content is never recorded** (a frame
//! may echo mail text or a secret); only the parser's reason and the byte
//! length survive. Agent-supplied strings (tool names, record types) pass
//! [`crate::supervise::scrub_detail`] and a length cap before reaching a
//! trace. Every per-life collection is bounded, with the uncapped total
//! preserved in `session-stream-summary` — so flooding the stream can
//! neither grow memory nor hide evidence.
//!
//! # Clock policy (CONVENTIONS §3 r4)
//!
//! No wall-clock reads inside the extractor: every `at_unix_ms` is supplied
//! by the caller, so extraction is a pure function of (events, timestamps)
//! and golden-testable without a runtime.
//!
//! [`parse_agent_event`]: hamlet_core::session::parse_agent_event

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use hamlet_core::session::{AgentEvent, CorrelationId, Cost, Disposition, Usage};

use crate::supervise::scrub_detail;
use crate::telemetry::{AgentKey, LatencySpike, SessionKey, Telemetry, TokenTally, TraceRecord};

/// Observation channel capacity per session. Generous: the sink folds whole
/// batches into one sled write pair, so the queue drains far faster than a
/// stream can fill it.
pub const OBSERVATION_CAPACITY: usize = 1024;

/// Max observations one sink wake folds into a single batch (bounds one
/// turn of the sink loop so a firehose cannot monopolize the runtime).
const SINK_BATCH_LIMIT: usize = 4096;

/// How often the life task retries a flush that the channel refused
/// (ADR-0018 cl. 1 — the bound on how stale `metrics_for` may be).
///
/// Pushing is non-blocking by design (telemetry must never stall the
/// delivery writer), so a full channel leaves state coalesced in the handle.
/// Retrying only when the *next frame* arrives is not enough: a stream that
/// goes quiet at a task boundary would leave its accounting invisible to
/// `metrics_for` until the life ends — and the query surface is meant to be
/// live (SPEC §11.2; CLI 08 / WS 09). The life task therefore arms a tick
/// guarded by [`SessionMetrics::has_pending`], which bounds staleness to
/// this interval and costs nothing while idle.
pub const FLUSH_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on locally coalesced append-only records awaiting a full channel.
const MAX_PENDING_RECORDS: usize = 64;
/// Bound on open tool-call spans (eviction is FIFO = oldest span).
const MAX_TOOL_SPANS: usize = 256;
/// Bound on wire ids awaiting an acknowledgment (FIFO eviction).
const MAX_PENDING_ACKS: usize = 64;
/// Malformed frames traced individually per life; the rest are counted and
/// reported in the summary (+ one suppression trace).
const MAX_MALFORMED_TRACES: u64 = 8;
/// Tool-call markers traced per life; all calls are counted regardless.
const MAX_TOOL_TRACES: u64 = 64;
/// Distinct unknown record types tracked per life (pi drift signal).
const MAX_UNKNOWN_TYPES: usize = 32;
/// Cap on an agent-supplied token (tool name, record type) inside a trace.
const NAME_CAP: usize = 64;

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

/// One extracted fact, ready for the sled sink. Additive variants
/// ([`Observation::Tokens`], [`Observation::CostMicro`]) coalesce losslessly
/// under backpressure; record variants are bounded and may be dropped
/// (counted) — evidence degrades, accounting never does.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// Token delta for `tokens.v1`.
    Tokens(TokenTally),
    /// Cost delta (micro-USD) for `costs.v1`.
    CostMicro(u64),
    /// Latency spike for `latency.v1`.
    Spike(LatencySpike),
    /// Trace event for `traces.v1`.
    Trace(TraceRecord),
}

/// Per-life stream counters. All monotonic; every one is reported in the
/// `session-stream-summary` trace, including the totals that bounded
/// per-frame tracing deliberately capped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamStats {
    /// Frames parsed (every stdout line, well-formed or not).
    pub frames: u64,
    /// Frames that were not valid JSONL records (non-blank).
    pub malformed_frames: u64,
    /// Blank frames (framing noise; counted, never traced).
    pub blank_frames: u64,
    /// Well-formed records of a type hamlet does not model (`Other`).
    pub unknown_records: u64,
    /// `response` records seen.
    pub responses: u64,
    /// `response` records with `success: false`.
    pub failed_responses: u64,
    /// Responses joined to a wire id this daemon wrote (ack latency).
    pub acks: u64,
    /// Low-level runs (`agent_start`) seen.
    pub runs: u64,
    /// Turns (`turn_start`) seen.
    pub turns: u64,
    /// Tool executions completed.
    pub tool_calls: u64,
    /// Tool executions that reported `isError`.
    pub tool_errors: u64,
    /// Prompt/input tokens committed to `tokens.v1`.
    pub input_tokens: u64,
    /// Completion/output tokens committed to `tokens.v1`.
    pub output_tokens: u64,
    /// Cache-read tokens (counted only — not in the `tokens.v1` shape).
    pub cache_read_tokens: u64,
    /// Cache-write tokens (counted only).
    pub cache_write_tokens: u64,
    /// pi's own `totalTokens` (counted only; cross-checks in+out+cache).
    pub total_tokens: u64,
    /// Micro-USD committed to `costs.v1`.
    pub cost_micro_usd: u64,
    /// Latency spikes recorded.
    pub spikes: u64,
    /// Observations dropped because both the channel and the local
    /// coalescing buffer were full (records only; accounting is lossless).
    pub dropped_observations: u64,
    /// Bounded-collection evictions (tool spans / pending acks).
    pub evictions: u64,
}

impl StreamStats {
    /// Deterministic `k=v` rendering for the summary trace (fixed field
    /// order, every field present — golden-testable).
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "frames={} malformed={} blank={} unknown={} responses={} failed={} acks={} \
             runs={} turns={} tools={} tool_errors={} tokens_in={} tokens_out={} \
             cache_read={} cache_write={} total_tokens={} cost_micro_usd={} spikes={} \
             dropped={} evictions={}",
            self.frames,
            self.malformed_frames,
            self.blank_frames,
            self.unknown_records,
            self.responses,
            self.failed_responses,
            self.acks,
            self.runs,
            self.turns,
            self.tool_calls,
            self.tool_errors,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.total_tokens,
            self.cost_micro_usd,
            self.spikes,
            self.dropped_observations,
            self.evictions
        )
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (golden-tested)
// ---------------------------------------------------------------------------

/// USD → integer micro-USD. `total` wins when it is finite and positive;
/// otherwise the four components are summed (some providers report only the
/// breakdown). Rounding is half-away-from-zero; NaN/infinite/negative
/// inputs are 0 (never poison a counter), and the result saturates.
#[must_use]
pub fn cost_micro_usd(cost: &Cost) -> u64 {
    let usd = if cost.total.is_finite() && cost.total > 0.0 {
        cost.total
    } else {
        let parts = [cost.input, cost.output, cost.cache_read, cost.cache_write];
        if parts.iter().all(|v| v.is_finite() && *v >= 0.0) {
            parts.iter().sum()
        } else {
            0.0
        }
    };
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    let scaled = usd * 1_000_000.0;
    if scaled >= u64::MAX as f64 {
        return u64::MAX;
    }
    scaled.round().max(0.0) as u64
}

/// Everything one cumulative usage snapshot adds since the previous one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UsageDelta {
    tokens: TokenTally,
    cost_micro_usd: u64,
    cache_read: u64,
    cache_write: u64,
    total_tokens: u64,
}

/// Field-wise delta between two cumulative usage snapshots. A snapshot
/// whose `totalTokens` regressed means the provider opened a new message
/// without a boundary frame: the whole snapshot is the delta and the
/// baseline rebases, so counters can never go negative or stall.
fn usage_delta(baseline: &Usage, next: &Usage) -> UsageDelta {
    let monotonic = next.total_tokens >= baseline.total_tokens;
    let step = |value: u64, base: u64| {
        if monotonic {
            value.saturating_sub(base)
        } else {
            value
        }
    };
    let cost = cost_micro_usd(&next.cost);
    UsageDelta {
        tokens: TokenTally {
            input_tokens: step(next.input, baseline.input),
            output_tokens: step(next.output, baseline.output),
        },
        cost_micro_usd: if monotonic {
            cost.saturating_sub(cost_micro_usd(&baseline.cost))
        } else {
            cost
        },
        cache_read: step(next.cache_read, baseline.cache_read),
        cache_write: step(next.cache_write, baseline.cache_write),
        total_tokens: step(next.total_tokens, baseline.total_tokens),
    }
}

/// Scrub + cap an agent-supplied token (tool name, record type) for trace
/// detail. Never used as a filesystem name — telemetry values only.
fn scrub_name(raw: &str) -> String {
    let scrubbed = scrub_detail(raw);
    scrubbed.chars().take(NAME_CAP).collect()
}

/// What a `response` id tells us about who it acknowledges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckKind {
    /// `m-`/`t-`/`n-` correlation: a queued delivery.
    Delivery,
    /// `i-` correlation: a Divine Interruption.
    Interrupt,
}

/// Stable lowercase label for a disposition (trace detail; `Disposition`
/// has no `Display` and its `Debug` render is not a wire vocabulary).
fn disposition_label(disposition: &Disposition) -> String {
    match disposition {
        Disposition::Started => "started".to_owned(),
        Disposition::Queued => "queued".to_owned(),
        Disposition::Handled => "handled".to_owned(),
        Disposition::Other(raw) => scrub_name(raw),
    }
}

/// Split a wire id (`<corr>:<suffix>`) into its acknowledgment kind,
/// correlation and record suffix. Ids this daemon did not mint (`rdy-…`,
/// pi-internal ids) yield `None`.
fn classify_wire_id(id: &str) -> Option<(AckKind, &str, &str)> {
    let (corr, suffix) = id.rsplit_once(':')?;
    if suffix.len() != 1 || !suffix.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    // The correlation charset check rejects anything we could not have
    // minted, so a hostile agent cannot forge ack telemetry for arbitrary
    // ids (it can still echo a real id — which is a genuine ack).
    let kind = match corr.as_bytes().first()? {
        b'i' => AckKind::Interrupt,
        b'm' | b't' | b'n' => AckKind::Delivery,
        _ => return None,
    };
    if CorrelationId::parse(corr).is_err() {
        return None;
    }
    Some((kind, corr, suffix))
}

// ---------------------------------------------------------------------------
// Extractor (pure fold)
// ---------------------------------------------------------------------------

/// Extractor configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractorConfig {
    /// A span at or above this many ms is a latency spike
    /// (`sessions.latency_spike_ms`).
    pub latency_spike_ms: u64,
}

impl ExtractorConfig {
    /// Config from a threshold in ms.
    #[must_use]
    pub const fn new(latency_spike_ms: u64) -> Self {
        Self { latency_spike_ms }
    }
}

/// An open latency span.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    started_at: u64,
    operation: &'static str,
    /// Extra detail for the spike-adjacent trace (empty for most spans).
    detail: String,
}

/// The pure stream fold: feed it every parsed frame with a caller-supplied
/// timestamp and it emits [`Observation`]s. No I/O, no clock, no sled.
#[derive(Debug)]
pub struct StreamExtractor {
    config: ExtractorConfig,
    stats: StreamStats,
    /// Cumulative usage baseline for the in-flight assistant message.
    usage_baseline: Option<Usage>,
    /// Open `agent-run` span (`agent_start`→`agent_end`).
    run_span: Option<Span>,
    /// Open `agent-task` span (first `agent_start`→`agent_settled`).
    task_span: Option<Span>,
    /// Open `agent-turn` span.
    turn_span: Option<Span>,
    /// Timestamp of the previous frame (for `stream-gap`).
    last_frame_at: Option<u64>,
    /// Open tool spans, FIFO: `(toolCallId, span)`.
    tool_spans: VecDeque<(String, Span)>,
    /// Wire ids awaiting a response, FIFO: `(wire_id, sent_at)`.
    pending_acks: VecDeque<(String, u64)>,
    /// Distinct unknown record types → count (bounded).
    unknown_types: BTreeMap<String, u64>,
    malformed_traced: u64,
    tool_traced: u64,
}

impl StreamExtractor {
    /// Extractor with a spike threshold.
    #[must_use]
    pub fn new(config: ExtractorConfig) -> Self {
        Self {
            config,
            stats: StreamStats::default(),
            usage_baseline: None,
            run_span: None,
            task_span: None,
            turn_span: None,
            last_frame_at: None,
            tool_spans: VecDeque::new(),
            pending_acks: VecDeque::new(),
            unknown_types: BTreeMap::new(),
            malformed_traced: 0,
            tool_traced: 0,
        }
    }

    /// Counters so far (a copy; the summary trace renders the same).
    #[must_use]
    pub const fn stats(&self) -> StreamStats {
        self.stats
    }

    /// The spike threshold in force.
    #[must_use]
    pub const fn latency_spike_ms(&self) -> u64 {
        self.config.latency_spike_ms
    }

    /// Record that this daemon wrote `wire_id` at `at_unix_ms`, so the
    /// matching `response` can be measured (`delivery-ack`/`interrupt-ack`).
    pub fn note_sent(&mut self, wire_id: &str, at_unix_ms: u64) {
        if self.pending_acks.len() >= MAX_PENDING_ACKS {
            self.pending_acks.pop_front();
            self.stats.evictions += 1;
        }
        self.pending_acks
            .push_back((wire_id.to_owned(), at_unix_ms));
    }

    /// Fold one parsed frame. `at_unix_ms` is caller-supplied (clock policy:
    /// CONVENTIONS §3 r4). Appends observations to `out` — the caller owns
    /// the buffer so the hot path allocates nothing.
    pub fn observe(&mut self, at_unix_ms: u64, event: &AgentEvent, out: &mut Vec<Observation>) {
        self.stats.frames += 1;
        self.stream_gap(at_unix_ms, out);
        self.last_frame_at = Some(at_unix_ms);

        match event {
            AgentEvent::MessageUpdate { usage: Some(usage) } => self.commit_usage(usage, out),
            AgentEvent::MessageUpdate { usage: None } => {}
            AgentEvent::AgentStart => {
                self.stats.runs += 1;
                self.close_run(at_unix_ms, out);
                self.run_span = Some(Span {
                    started_at: at_unix_ms,
                    operation: "agent-run",
                    detail: String::new(),
                });
                if self.task_span.is_none() {
                    self.task_span = Some(Span {
                        started_at: at_unix_ms,
                        operation: "agent-task",
                        detail: String::new(),
                    });
                }
            }
            AgentEvent::AgentEnd { will_retry } => {
                self.close_run(at_unix_ms, out);
                if *will_retry {
                    self.push_trace(at_unix_ms, "session-agent-retry", "pi will retry", out);
                }
            }
            AgentEvent::AgentSettled => {
                self.close_run(at_unix_ms, out);
                if let Some(span) = self.task_span.take() {
                    self.evaluate_span(at_unix_ms, span, out);
                }
                self.usage_baseline = None;
            }
            AgentEvent::TurnStart => {
                self.stats.turns += 1;
                self.close_turn(at_unix_ms, out);
                self.turn_span = Some(Span {
                    started_at: at_unix_ms,
                    operation: "agent-turn",
                    detail: String::new(),
                });
            }
            AgentEvent::TurnEnd => {
                self.close_turn(at_unix_ms, out);
                self.usage_baseline = None;
            }
            AgentEvent::Response {
                id,
                command,
                success,
                error,
                disposition,
            } => {
                self.stats.responses += 1;
                if !success {
                    self.stats.failed_responses += 1;
                }
                self.response(
                    at_unix_ms,
                    id.as_deref(),
                    command,
                    *success,
                    error.as_deref(),
                    disposition,
                    out,
                );
            }
            AgentEvent::Other { record_type, raw } => {
                self.other_record(at_unix_ms, record_type, raw, out);
            }
            AgentEvent::Malformed { line, reason } => {
                if line.trim().is_empty() {
                    self.stats.blank_frames += 1;
                    return; // framing noise: counted, never traced
                }
                self.stats.malformed_frames += 1;
                if self.malformed_traced < MAX_MALFORMED_TRACES {
                    self.malformed_traced += 1;
                    // Never the frame content (it may echo mail text or a
                    // secret — CONVENTIONS §6): reason + byte length only.
                    self.push_trace(
                        at_unix_ms,
                        "session-stream-malformed",
                        format!("{} bytes · {}", line.len(), scrub_name(reason)),
                        out,
                    );
                }
            }
        }
    }

    /// Close every open span and emit the per-life rollup. Called once when
    /// the stream ends (EOF, exit, kill) so a session that died mid-task
    /// still records the span that was open when it died.
    pub fn finish(&mut self, at_unix_ms: u64, out: &mut Vec<Observation>) {
        self.close_run(at_unix_ms, out);
        if let Some(span) = self.task_span.take() {
            self.evaluate_span(at_unix_ms, span, out);
        }
        self.close_turn(at_unix_ms, out);
        while let Some((_, span)) = self.tool_spans.pop_front() {
            self.evaluate_span(at_unix_ms, span, out);
        }
        self.pending_acks.clear();
        if self.stats.malformed_frames > self.malformed_traced {
            self.push_trace(
                at_unix_ms,
                "session-stream-malformed-suppressed",
                format!(
                    "{} further malformed frame(s) counted, not traced (cap {})",
                    self.stats.malformed_frames - self.malformed_traced,
                    MAX_MALFORMED_TRACES
                ),
                out,
            );
        }
        let unknown = if self.unknown_types.is_empty() {
            String::new()
        } else {
            let listed: Vec<String> = self
                .unknown_types
                .iter()
                .map(|(ty, n)| format!("{}:{n}", scrub_name(ty)))
                .collect();
            format!(" unknown_types=[{}]", listed.join(","))
        };
        self.push_trace(
            at_unix_ms,
            "session-stream-summary",
            format!("{}{}", self.stats.render(), unknown),
            out,
        );
    }

    // --- usage ------------------------------------------------------------

    /// Commit one cumulative usage snapshot as a delta (ADR-0016 cl. 2).
    fn commit_usage(&mut self, usage: &Usage, out: &mut Vec<Observation>) {
        let baseline = self.usage_baseline.unwrap_or_default();
        let delta = usage_delta(&baseline, usage);
        self.usage_baseline = Some(*usage);

        self.stats.input_tokens += delta.tokens.input_tokens;
        self.stats.output_tokens += delta.tokens.output_tokens;
        self.stats.cache_read_tokens += delta.cache_read;
        self.stats.cache_write_tokens += delta.cache_write;
        self.stats.total_tokens += delta.total_tokens;
        self.stats.cost_micro_usd += delta.cost_micro_usd;

        if delta.tokens.input_tokens > 0 || delta.tokens.output_tokens > 0 {
            out.push(Observation::Tokens(delta.tokens));
        }
        if delta.cost_micro_usd > 0 {
            out.push(Observation::CostMicro(delta.cost_micro_usd));
        }
    }

    // --- responses / acks -------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn response(
        &mut self,
        at_unix_ms: u64,
        id: Option<&str>,
        command: &str,
        success: bool,
        error: Option<&str>,
        disposition: &Option<Disposition>,
        out: &mut Vec<Observation>,
    ) {
        let Some(id) = id else { return };
        let Some((kind, corr, suffix)) = classify_wire_id(id) else {
            return; // pi-internal or readiness id: not ours to acknowledge
        };
        // Only ack what this daemon actually wrote (a hostile agent echoing
        // random ids gets no telemetry).
        let sent_at = self
            .pending_acks
            .iter()
            .rposition(|(wire, _)| wire == id)
            .map(|idx| {
                self.pending_acks
                    .remove(idx)
                    .expect("index from rposition")
                    .1
            });
        let Some(sent_at) = sent_at else { return };
        self.stats.acks += 1;

        let operation = match kind {
            AckKind::Interrupt => "interrupt-ack",
            AckKind::Delivery => "delivery-ack",
        };
        self.evaluate_span(
            at_unix_ms,
            Span {
                started_at: sent_at,
                operation,
                detail: String::new(),
            },
            out,
        );

        // Failures are already traced by the writer side (supervise's
        // `session-delivery-rejected` / `session-telemetry-failed`), which
        // owns the drain gate — metrics traces the positive ack only, so
        // every event keeps exactly one source.
        if !success {
            return;
        }
        let event = match kind {
            AckKind::Interrupt => "session-interrupt-acked",
            AckKind::Delivery => "session-delivery-acked",
        };
        let disposition = disposition
            .as_ref()
            .map_or_else(|| "none".to_owned(), disposition_label);
        let error = error.map_or_else(String::new, |e| format!(" error={}", scrub_name(e)));
        self.push_trace(
            at_unix_ms,
            event,
            format!("{corr} record={suffix} command={command} disposition={disposition}{error}"),
            out,
        );
    }

    // --- unmodelled records ----------------------------------------------

    fn other_record(
        &mut self,
        at_unix_ms: u64,
        record_type: &str,
        raw: &serde_json::Value,
        out: &mut Vec<Observation>,
    ) {
        match record_type {
            // Authoritative final usage for the assistant message.
            "message_end" => {
                if let Some(usage) = raw
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
                {
                    self.commit_usage(&usage, out);
                }
                self.usage_baseline = None;
            }
            "tool_execution_start" => {
                let call_id = raw
                    .get("toolCallId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let tool = raw
                    .get("toolName")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if self.tool_spans.len() >= MAX_TOOL_SPANS {
                    self.tool_spans.pop_front();
                    self.stats.evictions += 1;
                }
                self.tool_spans.push_back((
                    call_id.clone(),
                    Span {
                        started_at: at_unix_ms,
                        operation: "tool-call",
                        detail: scrub_name(tool),
                    },
                ));
                if self.tool_traced < MAX_TOOL_TRACES {
                    self.tool_traced += 1;
                    self.push_trace(
                        at_unix_ms,
                        "session-tool-call",
                        format!("{} id={}", scrub_name(tool), scrub_name(&call_id)),
                        out,
                    );
                }
            }
            "tool_execution_end" => {
                let call_id = raw
                    .get("toolCallId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let is_error = raw
                    .get("isError")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let index = self
                    .tool_spans
                    .iter()
                    .rposition(|(id, _)| id == call_id)
                    .map(|idx| self.tool_spans.remove(idx).expect("index from rposition"));
                self.stats.tool_calls += 1;
                if is_error {
                    self.stats.tool_errors += 1;
                }
                if let Some((_, mut span)) = index {
                    if is_error {
                        span.detail = format!("{} isError", span.detail);
                    }
                    self.evaluate_span(at_unix_ms, span, out);
                }
            }
            "compaction_start" | "compaction_end" | "auto_retry_start" | "auto_retry_end" => {
                self.push_trace(
                    at_unix_ms,
                    "session-stream-event",
                    scrub_name(record_type),
                    out,
                );
            }
            _ => {
                self.stats.unknown_records += 1;
                if self.unknown_types.len() < MAX_UNKNOWN_TYPES
                    || self.unknown_types.contains_key(record_type)
                {
                    *self
                        .unknown_types
                        .entry(record_type.to_owned())
                        .or_insert(0) += 1;
                    if self.unknown_types[record_type] == 1 {
                        // First sighting of an unmodelled record type: the
                        // pi-version-drift signal (HANDOFF step 02).
                        self.push_trace(
                            at_unix_ms,
                            "session-stream-unknown-record",
                            scrub_name(record_type),
                            out,
                        );
                    }
                }
            }
        }
    }

    // --- spans ------------------------------------------------------------

    /// `stream-gap`: a mid-task stall. Measured only while a task is in
    /// flight and no tool span is open — an idle session waiting for mail
    /// never spikes, and a slow tool is already attributed to its own
    /// `tool-call` span (attribution beats duplication).
    fn stream_gap(&mut self, at_unix_ms: u64, out: &mut Vec<Observation>) {
        let (Some(last), true, true) = (
            self.last_frame_at,
            self.task_span.is_some(),
            self.tool_spans.is_empty(),
        ) else {
            return;
        };
        self.evaluate_span(
            at_unix_ms,
            Span {
                started_at: last,
                operation: "stream-gap",
                detail: String::new(),
            },
            out,
        );
    }

    /// Close the open `agent-run` span, if any.
    fn close_run(&mut self, at_unix_ms: u64, out: &mut Vec<Observation>) {
        if let Some(span) = self.run_span.take() {
            self.evaluate_span(at_unix_ms, span, out);
        }
    }

    /// Close the open `agent-turn` span, if any.
    fn close_turn(&mut self, at_unix_ms: u64, out: &mut Vec<Observation>) {
        if let Some(span) = self.turn_span.take() {
            self.evaluate_span(at_unix_ms, span, out);
        }
    }

    fn evaluate_span(&mut self, at_unix_ms: u64, span: Span, out: &mut Vec<Observation>) {
        let duration_ms = at_unix_ms.saturating_sub(span.started_at);
        if duration_ms < self.config.latency_spike_ms {
            return;
        }
        self.stats.spikes += 1;
        out.push(Observation::Spike(LatencySpike {
            at_unix_ms,
            duration_ms,
            operation: if span.detail.is_empty() {
                span.operation.to_owned()
            } else {
                format!("{} {}", span.operation, span.detail)
            },
        }));
    }

    fn push_trace(
        &mut self,
        at_unix_ms: u64,
        event: &str,
        detail: impl Into<String>,
        out: &mut Vec<Observation>,
    ) {
        out.push(Observation::Trace(TraceRecord {
            at_unix_ms,
            event: event.to_owned(),
            detail: detail.into(),
        }));
    }
}

// ---------------------------------------------------------------------------
// Sink (per-session task owning every sled write)
// ---------------------------------------------------------------------------

/// The per-session telemetry sink handle, owned by the life task.
///
/// Feed it every parsed frame ([`SessionMetrics::observe`]) and every wire
/// id the task writes ([`SessionMetrics::note_sent`]); call
/// [`SessionMetrics::finish`] when the stream ends. Neither call ever
/// blocks: observations are coalesced locally when the sink is behind, so
/// the delivery writer keeps its turn in the select loop.
///
/// Because coalesced state is invisible to `metrics_for` until it is
/// flushed, the owner must also call [`SessionMetrics::retry_flush`]
/// periodically while [`SessionMetrics::has_pending`] — the life task does
/// this on a [`FLUSH_RETRY_INTERVAL`] tick. Without it a stream that goes
/// quiet at a task boundary would not publish its final totals until the
/// life ends.
#[derive(Debug)]
pub struct SessionMetrics {
    extractor: StreamExtractor,
    tx: mpsc::Sender<Observation>,
    task: Option<JoinHandle<SinkReport>>,
    scratch: Vec<Observation>,
    /// Additive observations waiting for channel room (lossless).
    pending_tokens: TokenTally,
    pending_cost: u64,
    /// Append-only records waiting for channel room (bounded).
    pending_records: VecDeque<Observation>,
    agent: AgentKey,
}

/// What the sink task did over one session life (diagnostics; a sled
/// failure degrades telemetry and is reported here, never propagated into
/// supervision — ADR-0005 cl. 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkReport {
    /// Observations applied to sled.
    pub applied: u64,
    /// sled writes that failed.
    pub failures: u64,
    /// Final stream counters at sink shutdown.
    pub stats: StreamStats,
}

/// Spawn the sink task for one session life. The returned handle is owned
/// by the life task; the task ends when the handle is dropped or
/// [`SessionMetrics::finish`] closes the channel.
#[must_use]
pub fn spawn(
    telemetry: std::sync::Arc<Telemetry>,
    agent: AgentKey,
    session: SessionKey,
    config: ExtractorConfig,
) -> SessionMetrics {
    spawn_with_capacity(telemetry, agent, session, config, OBSERVATION_CAPACITY)
}

/// [`spawn`] with an explicit channel bound (tests force backpressure with
/// a capacity of 1).
fn spawn_with_capacity(
    telemetry: std::sync::Arc<Telemetry>,
    agent: AgentKey,
    session: SessionKey,
    config: ExtractorConfig,
    capacity: usize,
) -> SessionMetrics {
    let (tx, rx) = mpsc::channel(capacity);
    let task = tokio::spawn(sink_task(telemetry, agent, session, rx));
    SessionMetrics {
        extractor: StreamExtractor::new(config),
        tx,
        task: Some(task),
        scratch: Vec::new(),
        pending_tokens: TokenTally::default(),
        pending_cost: 0,
        pending_records: VecDeque::new(),
        agent,
    }
}

impl SessionMetrics {
    /// The telemetry scope of this session.
    #[must_use]
    pub const fn agent(&self) -> AgentKey {
        self.agent
    }

    /// Stream counters so far.
    #[must_use]
    pub const fn stats(&self) -> StreamStats {
        self.extractor.stats()
    }

    /// Fold one parsed frame and ship the observations (non-blocking).
    pub fn observe(&mut self, at_unix_ms: u64, event: &AgentEvent) {
        self.scratch.clear();
        self.extractor.observe(at_unix_ms, event, &mut self.scratch);
        let mut drained = std::mem::take(&mut self.scratch);
        self.ship(drained.drain(..));
        self.scratch = drained; // reuse the allocation across frames
    }

    /// Record that a wire id was written to the session's stdin now.
    pub fn note_sent(&mut self, wire_id: &str, at_unix_ms: u64) {
        self.extractor.note_sent(wire_id, at_unix_ms);
    }

    /// Close the stream: finish the fold (open spans + summary), flush
    /// everything coalesced, and await the sink's report. Idempotent-ish —
    /// a second call returns the default report.
    pub async fn finish(mut self, at_unix_ms: u64) -> SinkReport {
        let mut tail = std::mem::take(&mut self.scratch);
        self.extractor.finish(at_unix_ms, &mut tail);
        let stats = self.extractor.stats();
        self.ship(tail.drain(..));
        self.flush_blocking().await;
        drop(self.tx);
        let mut report = match self.task.take() {
            Some(task) => match task.await {
                Ok(report) => report,
                // QA05-03: the life task was aborted (`abandon()` / kill -9),
                // which cancels the sink mid-write. That is normal teardown,
                // not a panic — nothing is left to report and no failure
                // should be claimed.
                Err(e) if e.is_cancelled() => SinkReport::default(),
                Err(e) => {
                    eprintln!("hamlet-daemon: metrics sink panicked: {e}");
                    SinkReport {
                        failures: 1,
                        ..SinkReport::default()
                    }
                }
            },
            None => SinkReport::default(),
        };
        report.stats = stats;
        report
    }

    /// Non-blocking ship: additive observations coalesce into the pending
    /// accumulators (lossless), records queue up to [`MAX_PENDING_RECORDS`]
    /// and are then dropped with a count (evidence degrades, accounting
    /// does not — ADR-0005 cl. 6).
    fn ship(&mut self, observations: impl Iterator<Item = Observation>) {
        for observation in observations {
            match observation {
                Observation::Tokens(delta) => {
                    self.pending_tokens.input_tokens += delta.input_tokens;
                    self.pending_tokens.output_tokens += delta.output_tokens;
                }
                Observation::CostMicro(micro) => self.pending_cost += micro,
                record => {
                    if self.pending_records.len() >= MAX_PENDING_RECORDS {
                        self.pending_records.pop_front();
                        self.extractor.stats.dropped_observations += 1;
                    }
                    self.pending_records.push_back(record);
                }
            }
        }
        self.retry_flush();
    }

    /// Push as much pending state through the channel as it will take
    /// without blocking. Returns whether the sink is still reachable.
    fn try_flush(&mut self) -> FlushOutcome {
        loop {
            let Some(next) = self.take_next() else {
                return FlushOutcome::Drained;
            };
            match self.tx.try_send(next) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(back)) => {
                    self.restore(back);
                    return FlushOutcome::Full;
                }
                Err(mpsc::error::TrySendError::Closed(back)) => {
                    self.restore(back);
                    return FlushOutcome::Closed;
                }
            }
        }
    }

    /// The next pending observation, in accounting-first order: additive
    /// state (lossless, always retried first) then append-only records.
    fn take_next(&mut self) -> Option<Observation> {
        if self.pending_tokens.input_tokens > 0 || self.pending_tokens.output_tokens > 0 {
            return Some(Observation::Tokens(std::mem::take(
                &mut self.pending_tokens,
            )));
        }
        if self.pending_cost > 0 {
            let cost = self.pending_cost;
            self.pending_cost = 0;
            return Some(Observation::CostMicro(cost));
        }
        self.pending_records.pop_front()
    }

    /// Put a rejected observation back (front of the record queue, so
    /// ordering survives a full channel).
    fn restore(&mut self, observation: Observation) {
        match observation {
            Observation::Tokens(tokens) => {
                self.pending_tokens.input_tokens += tokens.input_tokens;
                self.pending_tokens.output_tokens += tokens.output_tokens;
            }
            Observation::CostMicro(micro) => self.pending_cost += micro,
            record => self.pending_records.push_front(record),
        }
    }

    /// Blocking flush used only at [`SessionMetrics::finish`], when the
    /// stream is already over and nothing is waiting to be delivered. A
    /// closed channel (sink gone) ends the wait instead of spinning.
    async fn flush_blocking(&mut self) {
        while self.has_pending() {
            if self.try_flush() == FlushOutcome::Closed {
                self.drop_pending();
                return;
            }
            // The sink is alive (we still hold the only sender) and drains
            // continuously, so this yields rather than spins.
            tokio::task::yield_now().await;
        }
    }

    /// Is there coalesced state waiting for channel room? The life task
    /// uses this to arm its flush-retry tick, so an idle session costs
    /// nothing and a quiet-after-burst session still publishes promptly.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending_tokens.input_tokens > 0
            || self.pending_tokens.output_tokens > 0
            || self.pending_cost > 0
            || !self.pending_records.is_empty()
    }

    /// Retry a previously refused flush (non-blocking). Called by the life
    /// task on its [`FLUSH_RETRY_INTERVAL`] tick so pending accounting
    /// reaches sled — and therefore `metrics_for` — without waiting for the
    /// next stream frame or the end of the life.
    pub fn retry_flush(&mut self) {
        if self.try_flush() == FlushOutcome::Closed {
            self.drop_pending();
        }
    }

    /// Account pending state that can never reach the sink (closed
    /// channel) so the loss is visible in the summary instead of silent.
    fn drop_pending(&mut self) {
        let mut lost = u64::try_from(self.pending_records.len()).unwrap_or(u64::MAX);
        self.pending_records.clear();
        if self.pending_tokens.input_tokens > 0 || self.pending_tokens.output_tokens > 0 {
            lost += 1;
        }
        if self.pending_cost > 0 {
            lost += 1;
        }
        self.pending_tokens = TokenTally::default();
        self.pending_cost = 0;
        self.extractor.stats.dropped_observations += lost;
    }
}

async fn sink_task(
    telemetry: std::sync::Arc<Telemetry>,
    agent: AgentKey,
    session: SessionKey,
    mut rx: mpsc::Receiver<Observation>,
) -> SinkReport {
    let mut report = SinkReport::default();
    let mut batch: Vec<Observation> = Vec::new();
    while let Some(first) = rx.recv().await {
        batch.push(first);
        while batch.len() < SINK_BATCH_LIMIT {
            match rx.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        apply_batch(&telemetry, agent, session, &mut batch, &mut report);
        batch.clear();
    }
    // Durability checkpoint (ADR-0005 cl. 6): flushed metrics survive a
    // `kill -9`; the restart test relies on it.
    if let Err(e) = telemetry.flush() {
        eprintln!("hamlet-daemon: metrics flush failed for {}: {e}", agent.uid);
        report.failures += 1;
    }
    report
}

/// Fold one batch into the fewest possible sled writes: all token deltas
/// into one `add_tokens`, all cost deltas into one counter bump, then the
/// append-only records in stream order.
fn apply_batch(
    telemetry: &Telemetry,
    agent: AgentKey,
    session: SessionKey,
    batch: &mut Vec<Observation>,
    report: &mut SinkReport,
) {
    let mut tokens = TokenTally::default();
    let mut cost = 0u64;
    for observation in batch.drain(..) {
        match observation {
            Observation::Tokens(delta) => {
                tokens.input_tokens += delta.input_tokens;
                tokens.output_tokens += delta.output_tokens;
            }
            Observation::CostMicro(micro) => cost += micro,
            Observation::Spike(spike) => {
                report.applied += 1;
                if let Err(e) = telemetry.record_latency_spike(session, spike) {
                    report.failures += 1;
                    eprintln!("hamlet-daemon: latency spike write failed: {e}");
                }
            }
            Observation::Trace(record) => {
                report.applied += 1;
                if let Err(e) = telemetry.trace_event(agent, record) {
                    report.failures += 1;
                    eprintln!("hamlet-daemon: trace write failed: {e}");
                }
            }
        }
    }
    if tokens.input_tokens > 0 || tokens.output_tokens > 0 {
        report.applied += 1;
        if let Err(e) = telemetry.add_tokens(session, tokens) {
            report.failures += 1;
            eprintln!("hamlet-daemon: token tally write failed: {e}");
        }
    }
    if cost > 0 {
        report.applied += 1;
        if let Err(e) = telemetry.add_cost_micro_usd(session, cost) {
            report.failures += 1;
            eprintln!("hamlet-daemon: cost write failed: {e}");
        }
    }
}

/// Outcome of one non-blocking flush attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushOutcome {
    /// Everything pending reached the channel.
    Drained,
    /// The channel is full; pending state is retained for the next attempt.
    Full,
    /// The sink is gone; pending state can never be delivered.
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamlet_core::session::parse_agent_event;
    use std::sync::Arc;

    const T0: u64 = 1_700_000_000_000;
    const AGENT: AgentKey = AgentKey {
        commission: 7,
        uid: 1001,
    };
    const SESSION: SessionKey = SessionKey::new(7, 1001, 1001);

    /// Fold a scripted stream (JSONL line + caller-supplied timestamp per
    /// frame) through the pure extractor — the golden-vector harness. Every
    /// frame goes through the REAL [`parse_agent_event`], so these tests
    /// also pin the extractor to the core parser's output shape.
    fn fold<S: AsRef<str>>(
        threshold_ms: u64,
        frames: &[(S, u64)],
    ) -> (StreamStats, Vec<Observation>) {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(threshold_ms));
        let mut out = Vec::new();
        for (line, at) in frames {
            extractor.observe(*at, &parse_agent_event(line.as_ref()), &mut out);
        }
        (extractor.stats(), out)
    }

    fn fold_with_finish<S: AsRef<str>>(
        threshold_ms: u64,
        frames: &[(S, u64)],
        finish_at: u64,
    ) -> (StreamStats, Vec<Observation>) {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(threshold_ms));
        let mut out = Vec::new();
        for (line, at) in frames {
            extractor.observe(*at, &parse_agent_event(line.as_ref()), &mut out);
        }
        extractor.finish(finish_at, &mut out);
        (extractor.stats(), out)
    }

    fn totals(observations: &[Observation]) -> (u64, u64, u64) {
        let mut input = 0;
        let mut output = 0;
        let mut cost = 0;
        for observation in observations {
            match observation {
                Observation::Tokens(tally) => {
                    input += tally.input_tokens;
                    output += tally.output_tokens;
                }
                Observation::CostMicro(micro) => cost += micro,
                _ => {}
            }
        }
        (input, output, cost)
    }

    fn spikes(observations: &[Observation]) -> Vec<(String, u64)> {
        observations
            .iter()
            .filter_map(|o| match o {
                Observation::Spike(spike) => Some((spike.operation.clone(), spike.duration_ms)),
                _ => None,
            })
            .collect()
    }

    fn trace_details(observations: &[Observation], event: &str) -> Vec<String> {
        observations
            .iter()
            .filter_map(|o| match o {
                Observation::Trace(record) if record.event == event => Some(record.detail.clone()),
                _ => None,
            })
            .collect()
    }

    /// A cumulative usage snapshot as pi wires it (`totalTokens` = the whole
    /// response, cost in USD).
    fn usage(input: u64, output: u64, total_usd: f64) -> String {
        format!(
            r#"{{"input":{input},"output":{output},"cacheRead":0,"cacheWrite":0,"totalTokens":{tokens},"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":{total_usd}}}}}"#,
            tokens = input + output
        )
    }

    /// pi's authoritative final assistant message (docs `json.md`).
    fn message_end(input: u64, output: u64, total_usd: f64) -> String {
        format!(
            r#"{{"type":"message_end","message":{{"role":"assistant","content":[],"usage":{}}}}}"#,
            usage(input, output, total_usd)
        )
    }

    fn update(input: u64, output: u64, total_usd: f64) -> String {
        format!(
            r#"{{"type":"message_update","usage":{}}}"#,
            usage(input, output, total_usd)
        )
    }

    // --- cost conversion ---------------------------------------------------

    /// Golden: USD floats → integer micro-USD (ADR-0016 cl. 3). `total`
    /// wins; the component sum is the fallback; hostile floats are 0.
    #[test]
    fn cost_micro_usd_golden() {
        let cost = |total, input, output| Cost {
            input,
            output,
            cache_read: 0.0,
            cache_write: 0.0,
            total,
        };
        assert_eq!(cost_micro_usd(&cost(0.000001, 0.0, 0.0)), 1);
        assert_eq!(cost_micro_usd(&cost(0.0125, 0.0, 0.0)), 12_500);
        assert_eq!(cost_micro_usd(&cost(1.0, 0.0, 0.0)), 1_000_000);
        // Round half away from zero.
        assert_eq!(cost_micro_usd(&cost(0.0000015, 0.0, 0.0)), 2);
        assert_eq!(cost_micro_usd(&cost(0.0000004, 0.0, 0.0)), 0);
        // No usable total → component sum.
        assert_eq!(cost_micro_usd(&cost(0.0, 0.000001, 0.000002)), 3);
        // Hostile floats never poison a counter.
        assert_eq!(cost_micro_usd(&cost(f64::NAN, 0.0, 0.0)), 0);
        assert_eq!(cost_micro_usd(&cost(f64::INFINITY, 0.0, 0.0)), 0);
        assert_eq!(cost_micro_usd(&cost(-1.0, 0.0, 0.0)), 0);
        assert_eq!(cost_micro_usd(&cost(0.0, f64::NAN, 0.0)), 0);
        assert_eq!(cost_micro_usd(&cost(f64::MAX, 0.0, 0.0)), u64::MAX);
        assert_eq!(cost_micro_usd(&Cost::default()), 0);
    }

    // --- token / cost accounting ------------------------------------------

    /// Golden: a scripted two-turn stream yields EXACT totals. Cumulative
    /// snapshots inside a message are differenced, `message_end` is
    /// authoritative, and each turn rebases.
    #[test]
    fn cumulative_usage_commits_exact_totals() {
        let frames: Vec<(String, u64)> = vec![
            (r#"{"type":"agent_start"}"#.to_owned(), T0),
            (r#"{"type":"turn_start"}"#.to_owned(), T0 + 10),
            (update(10, 2, 0.000001), T0 + 20),
            (update(10, 5, 0.000004), T0 + 30),
            (message_end(10, 5, 0.000004), T0 + 40),
            (r#"{"type":"turn_end"}"#.to_owned(), T0 + 50),
            // Second turn: a fresh assistant message, counters restart.
            (r#"{"type":"turn_start"}"#.to_owned(), T0 + 60),
            (update(7, 3, 0.000002), T0 + 70),
            (r#"{"type":"turn_end"}"#.to_owned(), T0 + 80),
            (
                r#"{"type":"agent_end","willRetry":false}"#.to_owned(),
                T0 + 90,
            ),
            (r#"{"type":"agent_settled"}"#.to_owned(), T0 + 100),
        ];
        let (stats, observations) = fold(60_000, &frames);
        // turn 1 = 10 in / 5 out / 4 micro; turn 2 = 7 in / 3 out / 2 micro.
        assert_eq!(totals(&observations), (17, 8, 6));
        assert_eq!(
            (
                stats.input_tokens,
                stats.output_tokens,
                stats.cost_micro_usd
            ),
            (17, 8, 6)
        );
        assert_eq!(stats.total_tokens, 15 + 10);
        assert_eq!(stats.runs, 1);
        assert_eq!(stats.turns, 2);
        assert!(spikes(&observations).is_empty(), "threshold not breached");
    }

    /// Regression guard for the headline bug this module exists to prevent:
    /// summing cumulative snapshots instead of differencing them.
    #[test]
    fn naive_summing_of_cumulative_snapshots_is_not_what_we_do() {
        let frames: Vec<(String, u64)> = vec![
            (r#"{"type":"agent_start"}"#.to_owned(), T0),
            (r#"{"type":"turn_start"}"#.to_owned(), T0 + 1),
            (update(10, 2, 0.0), T0 + 2),
            (update(10, 5, 0.0), T0 + 3),
            (update(10, 9, 0.0), T0 + 4),
            (r#"{"type":"turn_end"}"#.to_owned(), T0 + 5),
        ];
        let (stats, observations) = fold(60_000, &frames);
        // Final snapshot, not 2+5+9.
        assert_eq!(totals(&observations), (10, 9, 0));
        assert_eq!((stats.input_tokens, stats.output_tokens), (10, 9));
    }

    /// `message_end` repeats the final snapshot: committing it must add
    /// nothing on top of the `message_update` deltas.
    #[test]
    fn message_end_is_authoritative_without_double_counting() {
        let frames: Vec<(String, u64)> = vec![
            (r#"{"type":"turn_start"}"#.to_owned(), T0),
            (update(4, 6, 0.000010), T0 + 1),
            (message_end(4, 6, 0.000010), T0 + 2),
            (r#"{"type":"turn_end"}"#.to_owned(), T0 + 3),
        ];
        let (_, observations) = fold(60_000, &frames);
        assert_eq!(totals(&observations), (4, 6, 10));
    }

    /// A provider that restarts its counters without a boundary frame must
    /// not underflow the tally or stall it at the old high-water mark.
    #[test]
    fn usage_regression_rebases_without_underflow() {
        let frames: Vec<(String, u64)> = vec![
            (r#"{"type":"turn_start"}"#.to_owned(), T0),
            (update(50, 50, 0.000100), T0 + 1),
            // totalTokens regresses → a new message began.
            (update(3, 4, 0.000007), T0 + 2),
            (r#"{"type":"turn_end"}"#.to_owned(), T0 + 3),
        ];
        let (stats, observations) = fold(60_000, &frames);
        assert_eq!(totals(&observations), (53, 54, 107));
        assert_eq!(
            (
                stats.input_tokens,
                stats.output_tokens,
                stats.cost_micro_usd
            ),
            (53, 54, 107)
        );
    }

    /// Cache/reasoning tokens are counted for the summary but never written
    /// to `tokens.v1` (the Sprint-01 record shape is fixed).
    #[test]
    fn cache_tokens_are_counted_but_not_tallied() {
        let frame = r#"{"type":"message_update","usage":{"input":1,"output":2,"cacheRead":30,"cacheWrite":40,"totalTokens":103,"cost":{"total":0}}}"#;
        let (stats, observations) = fold(60_000, &[(frame, T0)]);
        assert_eq!(totals(&observations), (1, 2, 0));
        assert_eq!(stats.cache_read_tokens, 30);
        assert_eq!(stats.cache_write_tokens, 40);
        assert_eq!(stats.total_tokens, 103);
    }

    // --- latency spikes ----------------------------------------------------

    /// Golden: the threshold is inclusive (`>=`), and every span family is
    /// measured from caller-supplied timestamps.
    #[test]
    fn latency_spike_threshold_is_inclusive() {
        // Turn of exactly 100 ms with a 100 ms threshold → spike.
        let at_threshold = [
            (r#"{"type":"agent_start"}"#, T0),
            (r#"{"type":"turn_start"}"#, T0 + 1_000),
            (r#"{"type":"turn_end"}"#, T0 + 1_100),
        ];
        let (_, observations) = fold(100, &at_threshold);
        assert!(
            spikes(&observations).contains(&("agent-turn".to_owned(), 100)),
            "spikes: {:?}",
            spikes(&observations)
        );

        // One millisecond under → silent.
        let under = [
            (r#"{"type":"turn_start"}"#, T0 + 1_000),
            (r#"{"type":"turn_end"}"#, T0 + 1_099),
        ];
        let (_, observations) = fold(100, &under);
        assert!(spikes(&observations).is_empty());
    }

    /// Golden: the full operation vocabulary for one task with a tool call.
    #[test]
    fn every_span_family_is_measured() {
        let frames = [
            (r#"{"type":"agent_start"}"#, T0),
            (r#"{"type":"turn_start"}"#, T0 + 100),
            (
                r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{}}"#,
                T0 + 200,
            ),
            (
                r#"{"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash","isError":false}"#,
                T0 + 900,
            ),
            (r#"{"type":"turn_end"}"#, T0 + 1_000),
            (r#"{"type":"agent_end","willRetry":false}"#, T0 + 1_100),
            (r#"{"type":"agent_settled"}"#, T0 + 2_000),
        ];
        let (stats, observations) = fold(500, &frames);
        let mut spans = spikes(&observations);
        spans.sort();
        assert_eq!(
            spans,
            [
                ("agent-run".to_owned(), 1_100),
                ("agent-task".to_owned(), 2_000),
                ("agent-turn".to_owned(), 900),
                // The 900 ms between agent_end and agent_settled: the task
                // has not reached its boundary yet, so it is a stall. The
                // 700 ms inside the tool call is NOT also a gap (it is
                // attributed to the tool span alone).
                ("stream-gap".to_owned(), 900),
                ("tool-call bash".to_owned(), 700),
            ]
        );
        assert_eq!(stats.spikes, 5);
        assert_eq!(stats.tool_calls, 1);
        assert_eq!(stats.tool_errors, 0);
        assert_eq!(
            trace_details(&observations, "session-tool-call"),
            vec!["bash id=call_1".to_owned()]
        );
    }

    /// A tool that reports `isError` is counted and its spike labelled.
    #[test]
    fn tool_errors_are_counted_and_labelled() {
        let frames = [
            (
                r#"{"type":"tool_execution_start","toolCallId":"c9","toolName":"read"}"#,
                T0,
            ),
            (
                r#"{"type":"tool_execution_end","toolCallId":"c9","isError":true}"#,
                T0 + 900,
            ),
        ];
        let (stats, observations) = fold(100, &frames);
        assert_eq!(stats.tool_errors, 1);
        assert_eq!(
            spikes(&observations),
            vec![("tool-call read isError".to_owned(), 900)]
        );
    }

    /// `stream-gap` fires only while a task is in flight: idle silence is
    /// not a latency spike.
    #[test]
    fn stream_gap_only_counts_mid_task() {
        // Idle: a huge gap between frames outside any run → silent.
        let idle = [
            (r#"{"type":"agent_settled"}"#, T0),
            (r#"{"type":"agent_settled"}"#, T0 + 600_000),
        ];
        let (_, observations) = fold(1_000, &idle);
        assert!(spikes(&observations).is_empty(), "idle gap must not spike");

        // Mid-task: the same gap is a stall.
        let running = [
            (r#"{"type":"agent_start"}"#, T0),
            (r#"{"type":"turn_start"}"#, T0 + 1),
            (r#"{"type":"turn_end"}"#, T0 + 30_000),
        ];
        let (_, observations) = fold(1_000, &running);
        assert!(
            spikes(&observations).contains(&("stream-gap".to_owned(), 29_999)),
            "spikes: {:?}",
            spikes(&observations)
        );
    }

    /// A session killed mid-task still records the spans that were open —
    /// [`StreamExtractor::finish`] is the crash path's evidence.
    #[test]
    fn finish_closes_open_spans() {
        let frames = [
            (r#"{"type":"agent_start"}"#, T0),
            (r#"{"type":"turn_start"}"#, T0 + 100),
        ];
        let (_, observations) = fold_with_finish(1_000, &frames, T0 + 5_000);
        let mut spans = spikes(&observations);
        spans.sort();
        assert_eq!(
            spans,
            [
                ("agent-run".to_owned(), 5_000),
                ("agent-task".to_owned(), 5_000),
                ("agent-turn".to_owned(), 4_900),
            ]
        );
    }

    // --- malformed / unknown frames ---------------------------------------

    /// Garbage is counted, capped, and NEVER carries its content: a frame
    /// may echo mail text or a secret (CONVENTIONS §6).
    #[test]
    fn malformed_frames_never_leak_content() {
        let secret = "HAMLET_API_KEY=test-key-do-not-use";
        let garbage = format!(r#"{{"type": ### {secret} not json"#);
        let frames: Vec<(String, u64)> =
            (0..20).map(|index| (garbage.clone(), T0 + index)).collect();
        let (stats, observations) = fold_with_finish(60_000, &frames, T0 + 100);

        assert_eq!(stats.malformed_frames, 20);
        assert_eq!(stats.frames, 20);
        let details = trace_details(&observations, "session-stream-malformed");
        assert_eq!(details.len() as u64, MAX_MALFORMED_TRACES);
        for detail in &details {
            assert!(
                !detail.contains(secret) && !detail.contains("###"),
                "frame content leaked into a trace: {detail}"
            );
            assert!(detail.contains("bytes"), "detail: {detail}");
        }
        // The uncapped truth survives in the suppression + summary traces.
        let suppressed = trace_details(&observations, "session-stream-malformed-suppressed");
        assert_eq!(suppressed.len(), 1);
        assert!(suppressed[0].contains("12 further"), "{}", suppressed[0]);
        let summary = trace_details(&observations, "session-stream-summary");
        assert_eq!(summary.len(), 1);
        assert!(summary[0].contains("malformed=20"), "{}", summary[0]);
        assert!(!summary[0].contains(secret));
    }

    /// Blank lines are framing noise: counted, never traced.
    #[test]
    fn blank_frames_are_counted_not_traced() {
        let frames = [
            ("", T0),
            ("   ", T0 + 1),
            (r#"{"type":"turn_start"}"#, T0 + 2),
        ];
        let (stats, observations) = fold(60_000, &frames);
        assert_eq!(stats.blank_frames, 2);
        assert_eq!(stats.malformed_frames, 0);
        assert!(trace_details(&observations, "session-stream-malformed").is_empty());
    }

    /// An unmodelled record type is traced once (the pi-drift signal) and
    /// counted per type in the summary.
    #[test]
    fn unknown_record_types_are_traced_once_each() {
        let frames = [
            (r#"{"type":"queue_update","steering":[]}"#, T0),
            (r#"{"type":"queue_update","steering":[]}"#, T0 + 1),
            (
                r#"{"type":"thinking_level_changed","level":"high"}"#,
                T0 + 2,
            ),
        ];
        let (stats, observations) = fold_with_finish(60_000, &frames, T0 + 3);
        assert_eq!(stats.unknown_records, 3);
        assert_eq!(
            trace_details(&observations, "session-stream-unknown-record"),
            vec![
                "queue_update".to_owned(),
                "thinking_level_changed".to_owned()
            ]
        );
        let summary = trace_details(&observations, "session-stream-summary");
        assert!(
            summary[0].contains("unknown_types=[queue_update:2,thinking_level_changed:1]"),
            "{}",
            summary[0]
        );
    }

    /// Hostile record-type strings are scrubbed and capped before they reach
    /// a trace value.
    #[test]
    fn hostile_record_types_are_scrubbed() {
        let hostile = format!(r#"{}\u001b[31m"#, "x".repeat(400));
        let frames = [(format!(r#"{{"type":"{hostile}"}}"#), T0)];
        let (_, observations) = fold(60_000, &frames);
        let details = trace_details(&observations, "session-stream-unknown-record");
        assert_eq!(details.len(), 1);
        assert!(details[0].chars().count() <= NAME_CAP, "{}", details[0]);
        assert!(!details[0].contains('\u{1b}'));
    }

    // --- acknowledgments ---------------------------------------------------

    /// Golden: an interrupt's `<corr>:{q,s,a}` responses join back to the
    /// injection via the wire id, and a slow ack is a spike.
    #[test]
    fn interrupt_ack_joins_on_the_wire_id() {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(100));
        let mut out = Vec::new();
        extractor.note_sent("i-3-1001.0:s", T0);
        extractor.note_sent("i-3-1001.0:a", T0);
        extractor.observe(
            T0 + 500,
            &parse_agent_event(
                r#"{"type":"response","id":"i-3-1001.0:s","command":"steer","success":true,"data":{"disposition":"queued"}}"#,
            ),
            &mut out,
        );
        extractor.observe(
            T0 + 501,
            &parse_agent_event(
                r#"{"type":"response","id":"i-3-1001.0:a","command":"abort","success":true}"#,
            ),
            &mut out,
        );
        assert_eq!(
            trace_details(&out, "session-interrupt-acked"),
            vec![
                "i-3-1001.0 record=s command=steer disposition=queued".to_owned(),
                "i-3-1001.0 record=a command=abort disposition=none".to_owned(),
            ]
        );
        assert_eq!(
            spikes(&out),
            [
                ("interrupt-ack".to_owned(), 500),
                ("interrupt-ack".to_owned(), 501)
            ]
        );
        assert_eq!(extractor.stats().acks, 2);
    }

    /// A queued delivery acks as `delivery-ack` / `session-delivery-acked`.
    #[test]
    fn delivery_ack_uses_the_queued_vocabulary() {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(1_000));
        let mut out = Vec::new();
        extractor.note_sent("m-12-1001.0:p", T0);
        extractor.observe(
            T0 + 40,
            &parse_agent_event(
                r#"{"type":"response","id":"m-12-1001.0:p","command":"prompt","success":true,"data":{"disposition":"started"}}"#,
            ),
            &mut out,
        );
        assert_eq!(
            trace_details(&out, "session-delivery-acked"),
            vec!["m-12-1001.0 record=p command=prompt disposition=started".to_owned()]
        );
        assert!(spikes(&out).is_empty(), "40 ms is under the threshold");
    }

    /// Failures are the writer's to trace (it owns the drain gate) — the
    /// extractor counts them and stays silent, so no event has two sources.
    #[test]
    fn failed_responses_are_counted_not_retraced() {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(1));
        let mut out = Vec::new();
        extractor.note_sent("t-1-1001.0:p", T0);
        extractor.observe(
            T0 + 50,
            &parse_agent_event(
                r#"{"type":"response","id":"t-1-1001.0:p","command":"prompt","success":false,"error":"agent is streaming"}"#,
            ),
            &mut out,
        );
        assert_eq!(extractor.stats().failed_responses, 1);
        assert!(trace_details(&out, "session-delivery-acked").is_empty());
        assert!(trace_details(&out, "session-interrupt-acked").is_empty());
        // The span is still measured: a slow refusal is still latency.
        assert_eq!(spikes(&out), vec![("delivery-ack".to_owned(), 50)]);
    }

    /// Responses for ids this daemon never wrote earn no ack telemetry —
    /// including the readiness probe and forged correlation shapes.
    #[test]
    fn foreign_response_ids_produce_no_ack() {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(1));
        let mut out = Vec::new();
        for id in [
            "rdy-1001.0",
            "i-9-1001.0:s",     // plausible, but never sent
            "i-9-1001.0:st",    // suffix must be one character
            "x-1-1001.0:p",     // not a hamlet kind prefix
            "i-1-1001.0:p bad", // correlation charset violation
            "",
        ] {
            extractor.observe(
                T0,
                &parse_agent_event(&format!(
                    r#"{{"type":"response","id":"{id}","command":"prompt","success":true}}"#
                )),
                &mut out,
            );
        }
        assert_eq!(extractor.stats().responses, 6);
        assert_eq!(extractor.stats().acks, 0);
        assert!(out.is_empty(), "observations: {out:?}");
    }

    // --- bounds ------------------------------------------------------------

    /// A firehose of unterminated tool spans is bounded: FIFO eviction,
    /// counted, no growth.
    #[test]
    fn tool_spans_are_bounded() {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(60_000));
        let mut out = Vec::new();
        let flood = MAX_TOOL_SPANS + 10;
        for index in 0..flood {
            let line = format!(
                r#"{{"type":"tool_execution_start","toolCallId":"c{index}","toolName":"bash"}}"#
            );
            extractor.observe(T0 + index as u64, &parse_agent_event(&line), &mut out);
        }
        assert_eq!(extractor.tool_spans.len(), MAX_TOOL_SPANS);
        assert_eq!(extractor.stats().evictions, 10);
        // Tool markers are capped too, while the count is not.
        assert_eq!(
            trace_details(&out, "session-tool-call").len() as u64,
            MAX_TOOL_TRACES
        );
        // Closing a surviving span still works after eviction.
        out.clear();
        let last = format!(
            r#"{{"type":"tool_execution_end","toolCallId":"c{}"}}"#,
            flood - 1
        );
        extractor.observe(T0 + 999_999, &parse_agent_event(&last), &mut out);
        assert_eq!(extractor.stats().tool_calls, 1);
    }

    /// Pending acks are bounded, so an agent that never answers cannot grow
    /// the extractor.
    #[test]
    fn pending_acks_are_bounded() {
        let mut extractor = StreamExtractor::new(ExtractorConfig::new(1));
        for index in 0..(MAX_PENDING_ACKS + 5) {
            extractor.note_sent(&format!("m-{index}-1001.0:p"), T0);
        }
        assert_eq!(extractor.pending_acks.len(), MAX_PENDING_ACKS);
        assert_eq!(extractor.stats().evictions, 5);
    }

    // --- summary -----------------------------------------------------------

    /// Golden: the summary detail rendering is a fixed `k=v` sequence.
    #[test]
    fn summary_render_is_golden() {
        let stats = StreamStats {
            frames: 12,
            malformed_frames: 2,
            blank_frames: 1,
            unknown_records: 3,
            responses: 4,
            failed_responses: 1,
            acks: 2,
            runs: 1,
            turns: 2,
            tool_calls: 3,
            tool_errors: 1,
            input_tokens: 17,
            output_tokens: 8,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 25,
            cost_micro_usd: 6,
            spikes: 1,
            dropped_observations: 0,
            evictions: 0,
        };
        assert_eq!(
            stats.render(),
            "frames=12 malformed=2 blank=1 unknown=3 responses=4 failed=1 acks=2 \
             runs=1 turns=2 tools=3 tool_errors=1 tokens_in=17 tokens_out=8 \
             cache_read=0 cache_write=0 total_tokens=25 cost_micro_usd=6 spikes=1 \
             dropped=0 evictions=0"
        );
    }

    #[test]
    fn wire_id_classification_is_exactly_the_hamlet_shape() {
        assert_eq!(
            classify_wire_id("i-3-1001.0:a"),
            Some((AckKind::Interrupt, "i-3-1001.0", "a"))
        );
        assert_eq!(
            classify_wire_id("n-1-1000.2:p"),
            Some((AckKind::Delivery, "n-1-1000.2", "p"))
        );
        assert_eq!(classify_wire_id("rdy-1001.0"), None);
        assert_eq!(classify_wire_id("i-3-1001.0:"), None);
        assert_eq!(classify_wire_id("i-3-1001.0:1"), None);
        assert_eq!(classify_wire_id("no-colon"), None);
    }

    // --- sink / backpressure ----------------------------------------------

    /// The soak invariant in miniature: with a capacity-1 channel and a sink
    /// that never gets scheduled (current-thread runtime, no await between
    /// pushes), every token and every micro-USD still lands in sled —
    /// additive observations coalesce instead of dropping.
    #[tokio::test(flavor = "current_thread")]
    async fn backpressure_coalesces_accounting_losslessly() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = std::sync::Arc::new(Telemetry::open(tmp.path().join("db")).unwrap());
        let mut sink = spawn_with_capacity(
            Arc::clone(&telemetry),
            AGENT,
            SESSION,
            ExtractorConfig::new(60_000),
            1,
        );
        for index in 1..=2_000u64 {
            // Growing cumulative snapshots: exactly 1 input + 1 output
            // token and 1 micro-USD per frame.
            let frame = update(index, index, f64::from(index as u32) * 0.000001);
            sink.observe(T0 + index, &parse_agent_event(&frame));
        }
        // Nothing was delivered yet (no await point): it is all coalesced.
        assert!(sink.pending_tokens.input_tokens > 0 || sink.pending_cost > 0);
        let report = sink.finish(T0 + 10_000).await;

        assert_eq!(report.failures, 0);
        assert_eq!(report.stats.input_tokens, 2_000);
        assert_eq!(report.stats.output_tokens, 2_000);
        assert_eq!(report.stats.cost_micro_usd, 2_000);
        assert_eq!(report.stats.dropped_observations, 0);
        assert_eq!(
            telemetry.session_tokens(SESSION).unwrap(),
            Some(TokenTally {
                input_tokens: 2_000,
                output_tokens: 2_000,
            })
        );
        assert_eq!(telemetry.session_cost_micro_usd(SESSION).unwrap(), 2_000);
    }

    /// Append-only records are bounded under backpressure and the loss is
    /// counted, never silent. (Per-life trace caps mean a single stream
    /// cannot produce this many records on its own — that is the point of
    /// the caps — so the bound is driven directly.)
    #[tokio::test(flavor = "current_thread")]
    async fn record_backpressure_is_bounded_and_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = std::sync::Arc::new(Telemetry::open(tmp.path().join("db")).unwrap());
        let mut sink = spawn_with_capacity(
            Arc::clone(&telemetry),
            AGENT,
            SESSION,
            ExtractorConfig::new(1),
            1,
        );
        let record = |index: usize| {
            Observation::Trace(TraceRecord {
                at_unix_ms: T0 + index as u64,
                event: "session-tool-call".to_owned(),
                detail: format!("bash id=call_{index}"),
            })
        };
        for index in 0..(MAX_PENDING_RECORDS * 4) {
            sink.ship(std::iter::once(record(index)));
            assert!(
                sink.pending_records.len() <= MAX_PENDING_RECORDS,
                "record queue grew without bound at {index}"
            );
        }
        let report = sink.finish(T0 + 999).await;
        assert!(
            report.stats.dropped_observations > 0,
            "loss must be counted: {:?}",
            report.stats
        );
        // Accounting is untouched by record pressure.
        assert_eq!(report.failures, 0);
    }

    /// Liveness of coalesced state (the bug behind QA's flaky flood test):
    /// pushing is non-blocking, so a full channel leaves accounting pending
    /// in the handle. Retrying only when the *next frame* arrives meant a
    /// stream that went quiet at a task boundary published a stale total to
    /// `metrics_for` until the life ended. `has_pending` + `retry_flush`
    /// are what the life task's tick arm uses to bound that staleness.
    #[tokio::test(flavor = "current_thread")]
    async fn pending_state_is_visible_and_retry_flush_publishes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = std::sync::Arc::new(Telemetry::open(tmp.path().join("db")).unwrap());
        let mut sink = spawn_with_capacity(
            Arc::clone(&telemetry),
            AGENT,
            SESSION,
            ExtractorConfig::new(60_000),
            1,
        );
        // Capacity 1 + a current-thread runtime: the sink cannot run until
        // we await, so everything past the first observation is coalesced.
        for index in 1..=500u64 {
            let frame = update(index, index, 0.0);
            sink.observe(T0 + index, &parse_agent_event(&frame));
        }
        assert!(sink.has_pending(), "coalesced state must be detectable");
        let partial = telemetry
            .session_tokens(SESSION)
            .unwrap()
            .unwrap_or_default();
        assert!(
            partial.input_tokens < 500,
            "expected the tail to be unflushed, got {partial:?}"
        );

        // Let the sink drain, then retry: exactly what the tick arm does.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            sink.retry_flush();
        }
        assert!(
            !sink.has_pending(),
            "retry_flush must clear pending state once there is room"
        );
        let report = sink.finish(T0 + 9_000).await;
        assert_eq!(report.stats.input_tokens, 500);
        assert_eq!(report.stats.dropped_observations, 0);
        assert_eq!(
            telemetry.session_tokens(SESSION).unwrap(),
            Some(TokenTally {
                input_tokens: 500,
                output_tokens: 500,
            }),
            "live totals must be exact without waiting for finish()"
        );
    }

    /// Dropping the handle without `finish` still ends the sink task (no
    /// leak) — the `abandon()`/kill-9 path.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_the_handle_ends_the_sink() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = std::sync::Arc::new(Telemetry::open(tmp.path().join("db")).unwrap());
        let mut sink = spawn(
            Arc::clone(&telemetry),
            AGENT,
            SESSION,
            ExtractorConfig::new(60_000),
        );
        sink.observe(T0, &parse_agent_event(&update(5, 5, 0.0)));
        drop(sink);
        // Yield until the sink task notices the closed channel and writes.
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if telemetry.session_tokens(SESSION).unwrap().is_some() {
                break;
            }
        }
        assert_eq!(
            telemetry.session_tokens(SESSION).unwrap(),
            Some(TokenTally {
                input_tokens: 5,
                output_tokens: 5
            })
        );
    }
}
