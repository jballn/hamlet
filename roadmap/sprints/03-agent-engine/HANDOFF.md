# HANDOFF — Sprint 03 Agent Engine

## GATE 03/07 — Merge & tag record (commit-agent)

- **Precondition:** `GATE 03/06: PASS` verified in this HANDOFF (section
  below).
- **Diff review (`git diff main...HEAD`):** 27 files, +14905/−128 — all
  source, tests, ADRs, roadmap, scripts, goldens. No secrets, no `.env`,
  no `target/`, no runtime `.hamlet/` trees, no sled dbs, no logs, no
  binaries (fakeagent.rs is a committed source bin, not an artifact).
  Secret-shaped grep hits are deliberate fixtures only: the fake value
  `sk-secret` in the session-exec argv golden and the `<redacted>` masking
  assertions + the `secrets_never_reach_files_charter_or_sled` byte-scan
  (SPEC §11.4, CONVENTIONS §6 — fixtures use obvious fakes).
  `crates/hamlet-cli/tests/golden/{charter,config}.json` carry only the
  additive v2/v4 session + latency-spike keys. Untracked `assets/` and
  `roadmap/.runner-state.json` left unstaged (runner scratch, not branch
  content).
- **DoD pre-check (CONVENTIONS §8):** gate 03/06 ran the full battery on
  the pre-merge sprint tree (`3fee968` + `6e0caf0` docs) → PASS; this gate
  re-ran it on merged main (below).
- **Squash-merge:** `sprint/03-agent-engine` (tip `6e0caf0`) → `main` as
  **`2063c52`** (`2063c52fdee9434211d1355281ce439705a42579`) —
  `feat(core,daemon): sprint 03 agent engine — pi session supervision,
  delivery queue, divine interruption, telemetry → sled`. Merged tree
  verified byte-identical to the sprint branch tip (`git diff 6e0caf0
  main` empty).
- **Tag:** `sprint/03-agent-engine` (annotated) at `2063c52`.
- **Post-merge CI on `main`:** `scripts/ci-local.sh` → **`CI battery:
  PASS`** (fmt, clippy `-D warnings`, 349 workspace tests across 31 ok
  runs, check-deps, e2e-01 ×2, e2e-02-isolation, e2e-03-adversarial incl.
  T1 12/12, T2 QA05-02 race 10/10 gated samples, T3 secrets ratchets, T4
  flush-wiring ratchet; guarded podman steps SKIP — no podman on this
  host, recorded as in gates 02/06 and 03/06; open issue carried to the
  podman-capable CI job). Log: `/tmp/ci-03-07-main.log`, exit 0.
- Open issue carried to Sprint 04: QA05-01 (capped incremental stdout
  reader) — ruled SHOULD-FIX, non-blocking.
- No amend, no force-push, shared history untouched.

**GATE 03/07: PASS** — merge SHA `2063c52`, tag `sprint/03-agent-engine`.

## GATE 03/06 — CI gate verdict: **PASS**

Precondition checked: `GATE 03/05: PASS` present (re-gate record below). Full
battery run via `scripts/ci-local.sh` (→ `scripts/ci.sh`) on commit `3fee968`;
log: `/tmp/ci-03-06.log`, exit 0.

| # | Check | Result | Evidence |
|---|-------|--------|----------|
| 1 | `cargo fmt --all --check` | **PASS** | ci.sh step 1/5, no diff |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | ci.sh step 2/5, zero warnings |
| 3 | `cargo test --workspace` | **PASS** | ci.sh step 3/5 |
| 4 | `scripts/check-deps.sh` | **PASS** | "all dependencies within the locked allowlist" |
| 5 | Guarded podman suite | **SKIP** | podman not on PATH on this host — ci.sh step 5/5 skips by design (absence must not fail the battery); `*_guarded.rs` tests are `#[ignore]`d + double-guarded (`HAMLET_PODMAN_TESTS=1` + podman PATH probe) |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | "boundary is type-enforced" |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | — |
| 8 | `scripts/e2e-02-isolation.sh` | **PASS** | T1 adversarial isolation (FakeRuntime) |
| 9 | `scripts/e2e-03-adversarial.sh` | **PASS** | T1 suite, T2 QA05-02 gate (10 samples), T3 secrets, T4 metrics-liveness |

Aggregate across all cargo runs in the battery: **349 passed, 0 failed** (31
`test result: ok` lines).

**ADR wire-format verification.** Sprint prompts cite "ADR-0008"; the actual
wire-schema ADR is **ADR-0015** (0008 is claimed by slot identity; the
aliasing is documented in ADR-0015's header note and docs/adr/README.md §
alias table — numbering rule respected). Golden spot-check against ADR-0015:
- cl. 2 envelopes — `envelope_json_golden_vectors` (hamlet-core/src/session.rs):
  `{"kind":"mail|tasking|notice","correlation":"<m|t|n>-<seq>-<uid>.<inc>","text":…}`
  and `{"kind":"interrupt_and_steer","correlation":"i-…","source":"system|human|mayor","priority":"normal|critical","steering_text":…}`
  — exact match, canonical JSONL, round-trip asserted.
- cl. 3 binding — `binding_queued_mail_golden` / `binding_interrupt_golden`:
  queued → single `prompt` id `<corr>:p` only in Running/Idle (Busy/Starting →
  typed `NotAtTaskBoundary`); interrupt busy → `steer(:s)` + `abort(:a)`,
  critical prefixes `clear_queue(:q)`; idle → header-marked prompt
  (`[hamlet-interrupt source=… priority=… corr=…]`); starting/dead → typed
  refusal. Readiness probe `rdy-<uid>.<inc>` golden present
  (`readiness_probe_golden`). **ADR-0015 matches the implementation.**

**FakeAgent verification.** `crates/hamlet-daemon/src/bin/fakeagent.rs` is
committed (git-tracked, ADR-0015 cl. 5 — no new workspace crate) and consumed
via `env!("CARGO_BIN_EXE_fakeagent")` by all four unguarded suites
(`supervise`, `delivery`, `metrics`, `adversarial_agent`) and the e2e-03
harness. No unguarded test depends on a live `pi` binary: real-pi usage is
confined to `supervise_guarded.rs` (`#[ignore]` + `HAMLET_PODMAN_TESTS=1` +
podman-on-PATH guard, CONVENTIONS §5); the four e2e scripts reference no live
pi. Guarded podman suite could not be exercised on this host (no podman) —
carried as SKIP, not FAIL, per ci.sh contract; hosts with podman run it via
step 5/5.

No product code was edited by this gate; no CI config changes were needed.

**GATE 03/06: PASS.**

## GATE 03/05 — QA re-run verdict: **PASS** (re-gated after the QA05-02/04 fixes)

QA re-verification of `6d30ef1` + `2518b64` (this section is the re-gate
record; the original FAIL verdict and findings follow below):

- **QA05-02 fix verified.** `drain_tail` (supervise.rs, `Step::Exit` arm)
  reads stdout to EOF after process exit, bounded by one stop-grace window
  (`timeout(ctx.stop_grace(), …)` — a hostile grandchild holding the pipe
  open cannot wedge the life task past the bound; overrun traces
  `session-stream-tail-truncated`), folds through telemetry + the pure
  core reducer, resets the restart budget at a tail boundary exactly like
  the live path, and never delivers (stdin gone; at-most-once preserved).
  The repro is un-ignored with its body unchanged from the version QA
  measured failing 6/15 pre-fix; QA re-sampled it 10× via e2e T2 and 30×
  ad-hoc post-fix: 0 losses.
- **QA05-04/ADR-0018 fix verified.** `FlushTick` is guarded by
  `has_pending()` (idle sessions pay nothing), re-pins after firing (no
  spin), and `ship()` routes through the single `retry_flush()` policy.
  Deterministic mechanism test + live-boundary property test committed.
- **QA05-05 test-side fixes reviewed and accepted** — every changed
  predicate strengthens the settle evidence; no assertion was weakened
  (the `Idle|Busy` relaxation in `crash_mid_task` is bounded by the
  restart-budget-reset wait, which only a real boundary can satisfy).
- **QA05-06 filed and fixed by QA during the re-gate** (harness-only; see
  finding below): e2e T2 was a FALSE PASS (zero-test match), and four
  scenarios still carried wire-log evidence races under parallel load
  (15/80 failing at 4× load before the fix; **0/170 after**, two
  independent samples: 4×20 and 3×30).
- Full battery: `./scripts/ci-local.sh` → **CI battery: PASS** (incl.
  e2e-03 T1 12/12, T2 10/10 real samples, T3 secrets ratchets, T4 flush
  wiring ratchet).

**GATE 03/05: PASS.** QA05-01 remains ruled SHOULD-FIX → Sprint 04
(capped incremental stdout reader); it is non-blocking by ruling.

### Original verdict (superseded): FAIL

QA found one product bug (QA05-02, telemetry tail loss at session exit — a
`tokio::select!` race, 6/15 repro rate). **Fixed** by `drain_tail` in
`supervise.rs` (step-04 follow-up commit): after the process exits, stdout
is read to EOF — bounded by one stop-grace window — folding every frame
through telemetry and the pure status reducer before `classify`. The repro
is un-ignored and now a committed regression test; `e2e-03-adversarial.sh`
T2 samples it 10× and **gates** on it (10/10 green; it was 6/15 failing
before the fix). QA05-03 (nit) fixed alongside. QA05-01 stands as ruled
(SHOULD-FIX → Sprint 04).

Chasing the residual flake in QA's suite then surfaced **two more findings**,
both recorded below and both fixed: **QA05-04** (a second *product* bug, in
step 04's own metrics sink — coalesced accounting stayed invisible to
`metrics_for` until the next frame or the end of the life) and **QA05-05**
(three *test-side* settle races in the step-05 harness, which asserted on
evidence before it had landed).

Adversarial-suite flake rate across the work: **4/30** at the QA commit →
**6/70** with `drain_tail` → **1/40** with the flush-retry tick → **0/120**
after QA05-05 (two independent 60-run samples). Full daemon suite 3× → 196
green each. **The gate verdict itself is QA's to re-issue.**

## QA findings — step 05 (adversarial suite)

### QA05-02 (BUG, blocking) — **FIXED** — stdout tail dropped when the exit arm wins the select race

- **file:line:** `crates/hamlet-daemon/src/supervise.rs:1859–1863`
  (`Step::Exit` returns `classify(...)` immediately) with the race set up
  at `supervise.rs:1663` (`BufReader::new(pipes.stdout).lines()`) and
  `supervise.rs:1705` (`line = reader.next_line()` arm of `tokio::select!`
  competing against `exit = &mut exit_rx`).
- **repro:** `cargo test -p hamlet-daemon --test adversarial_agent
  repro_qa05_02 -- --ignored` (run several times). A graceful stop lands
  while the agent is mid-run; the agent finishes flushing its frames and
  exits on the EOF; the reaper's `exit_rx` and the reader's remaining
  buffered lines are BOTH ready; `select!` may take the exit arm, and
  `Step::Exit` returns without draining — the buffered frames are never
  folded into telemetry. Measured: **6/15 runs lose the tail**.
- **expected:** every frame the agent emitted before exit is folded —
  `tokens_in == 3000` (the exact scripted total; ADR-0017 cl. 2 lossless
  accounting; SPEC §6 "daemon parses the JSON-RPC stream for token counts,
  cost, latency, and trace events → sled").
- **actual:** `tokens_in` short by the tail — observed values
  `2842…2998` instead of `3000`; trailing whole record blocks (e.g. all
  40 unknown-record frames of the final turn) vanish. Same race applies
  to the CRASH path (`Step::Exit` on non-zero code), so the frames
  describing a session's death — the most forensic tail — can be dropped.
- **fix sketch (product; NOT applied by QA):** after the process exits,
  drain `reader.next_line()` to `Ok(None)` (bounded by the existing stop
  grace) folding each line through `metrics.observe` + the supervision
  reducer, before `classify`. The `Step::Eof` path already drains fully;
  only the exit-first ordering loses data.
- **note:** committed suite avoids the window (waits for full parse before
  stopping), so `cargo test` stays green; the ignored repro is the
  executable finding.
- **FIX (step-04 follow-up):** `async fn drain_tail<R: AsyncBufRead + Unpin>`
  in `supervise.rs`, called from the `Step::Exit` arm before `classify` —
  so it covers the stop race **and** the crash path (both take that arm;
  the frames describing a death are the most forensic tail). It reads to
  `Ok(None)`, folding each line through `metrics.observe` +
  `SessionStatus::on_agent_event` (core's pure reducer — no duplication of
  the live loop's logic), and resets the restart budget if the tail
  reaches a boundary, exactly as the live path would. Two deliberate
  limits: (a) **bounded by one `stop_grace` window**, because normally the
  write end died with the process and every buffered line reads back
  instantly — the budget only matters when something else (a `podman exec`
  intermediate, an orphaned grandchild) still holds stdout open, which must
  not wedge the life task; exceeding it traces
  `session-stream-tail-truncated` rather than losing frames silently.
  (b) **No delivery in the tail** — stdin is gone, and popping an envelope
  only to fail the write would *lose* it (`pop` persists the removal before
  the write — at-most-once, `crate::delivery`). The `Step::Eof` arm needed
  no change: reaching EOF means the whole stream was already read.
  `GraceTimeout` is intentionally NOT drained — that arm fires only after
  the grace window was already exceeded, so adding another window would
  double the worst case of a path whose whole purpose is a forced kill.
- **verification:** `repro_qa05_02_stop_midrun_loses_telemetry_tail`
  un-ignored (per QA's instruction) → 25/25 green locally, and
  `e2e-03-adversarial.sh` T2 samples it 10× as a **gating** step. Added
  `crash_mid_flood_accounts_every_frame_and_closes_the_span`
  (`tests/metrics.rs`) pinning the crash-path arithmetic (frame budget
  1000 → exactly 996 tokens in/out and 996 micro-USD, `dropped=0`,
  `agent-task` spike from the span open at death). Honest caveat recorded
  there: that test passes with or without the fix (a 1000-frame flood
  reaches EOF first), so it guards crash-path *accounting*, not the race —
  the race is guarded by QA's repro on the shared `Step::Exit` arm.
- **regression watch:** the full daemon suite was run 3× after the fix
  (194 tests green each). One earlier parallel run showed a single
  `wait_for` 60 s timeout in `overflow_bounces_…`; it did not reproduce in
  any later run (serial 3.5 s, parallel 1.05 s, baseline 0.99 s) and cannot
  be caused by `drain_tail`, which is bounded at `stop_grace` (10 s) and
  runs only *after* the pre-stop condition that timed out.

### QA05-01 (carried from step 04, ruled) — oversized frame allocated before any size guard

- **file:line:** `crates/hamlet-daemon/src/supervise.rs:1663` +
  `1705` — `BufReader::lines()`/`next_line()` allocates the entire line
  before any cap applies.
- **repro:** `hostile_huge_frames_are_skipped_not_applied` (committed,
  passes) feeds 4 MiB garbage + 2 MiB truncated JSON frames: they ARE
  skipped/traced/never applied, session settles, totals exact, no
  restart, every trace detail < 512 bytes.
- **ruling:** the behavioral spec ("skipped/traced, never applied") is
  **met**. The residual risk is memory: an agent emitting one huge
  no-LF frame makes the daemon allocate it unbounded — a DoS surface on
  the singular governor. **Non-blocking for this gate** (agents are
  semi-trusted, the frame is transient not accumulating, and the fix
  touches the hot path 24 supervision tests depend on). **SHOULD-FIX,
  tracked to Sprint 04:** a capped incremental reader (e.g. 1 MiB → then
  count as malformed) closes it.

### QA05-03 (nit, non-blocking) — **FIXED** — sink-cancel message reads as a panic

- **file:line:** `crates/hamlet-daemon/src/metrics.rs:977`
  (`eprintln!("hamlet-daemon: metrics sink panicked: {e}")`).
- **repro:** any test that unwinds while a session is live prints
  `metrics sink panicked: task N was cancelled`.
- **expected vs actual:** task cancellation at teardown is normal, not a
  panic; the wording is alarming. Suggest distinguishing
  `JoinError::is_cancelled()`. Cosmetic.
- **FIX (step-04 follow-up):** `SessionMetrics::finish` now matches
  `Err(e) if e.is_cancelled()` → a silent default report (normal teardown
  after `abandon()`/kill -9, nothing left to report, no failure claimed),
  and only a genuine panic prints and counts as `failures: 1`.

### QA05-04 (BUG, product — step 04's metrics sink) — **FIXED**, decision recorded as ADR-0018 — coalesced metrics invisible to `metrics_for` at a quiet boundary

- **file:line:** `crates/hamlet-daemon/src/metrics.rs`
  (`SessionMetrics::ship` → `try_flush`); the missing retry was in the
  life-task select loop in `crates/hamlet-daemon/src/supervise.rs`.
- **symptom:** the flood scenarios failed intermittently (~9%: 6/70 runs) with
  `wait_for("flood fully parsed")` timing out at 60 s. Instrumented capture at
  the timeout: `queue_len=0 status=idle tokens_in=4454` — and the moment
  `stop_session` ran (→ `finish()` → blocking flush) the same query returned
  **4500**, while the life summary already read `tokens_in=4500 dropped=0`.
- **diagnosis:** pushing is non-blocking by design (ADR-0017 cl. 2 — telemetry
  must never stall the delivery writer), so when the observation channel is
  full the accounting coalesces into the handle's local accumulator. That
  accumulator was retried **only when the next frame arrived**. A stream that
  goes quiet at a task boundary therefore left its final totals unpublished
  until the life ended, so the *live* query surface (SPEC §11.2; the CLI/WS
  read model) served a stale total for an idle session. Nothing was lost
  (`dropped=0`) — it was invisible. **Not a QA test defect:** the test was right
  to expect a live read to be exact.
- **fix:** public `SessionMetrics::has_pending()` + `retry_flush()`, and a
  `Step::FlushTick` arm in the life task armed by
  `sleep(metrics::FLUSH_RETRY_INTERVAL)` (100 ms) **guarded by
  `if metrics.has_pending()`** — an idle session pays nothing (the arm is
  disabled, the elapsed timer never polled) while pending state publishes
  within one interval. The arm re-pins after firing, so it cannot spin without
  yielding (step-02 gotcha 2).
- **guards:** `metrics::tests::pending_state_is_visible_and_retry_flush_publishes_it`
  (deterministic mechanism test: capacity-1 channel, current-thread runtime);
  `tests/metrics.rs::live_totals_are_exact_at_a_boundary_without_stopping`
  (product property: exact totals while the session is **live and idle**, no
  `stop_session`); `e2e-03-adversarial.sh` **T4** source ratchet (verified to
  exit 1 when the guard is disabled). The stale-tail window needs the *sink*
  descheduled while the producer finishes, so it reproduces under parallel load
  rather than from flood volume alone — stated in the test's doc comment rather
  than overclaimed.

### QA05-05 (TEST-SIDE, not a product bug) — **FIXED** — three adversarial scenarios asserted on evidence before it landed

Found while proving QA05-04; once the product fixes landed, every residual
failure was a harness race. Each was confirmed against daemon-side evidence
**before** the test was touched, and every fix **strengthens** a settle
predicate — no assertion was weakened or removed.

- **`interrupt_storm_keeps_framing_and_resolves_correlations`** — settled on
  `queue_len() == 0 && status == Idle`, then read the wire log. But
  `QueueStore::pop` persists the removal *before* the stdin write (at-most-once,
  `crate::delivery`), so the predicate can go true inside the pop→write window.
  Captured proof the product was correct: for the "missing" mail `m-9` the
  traces showed `session-enqueued`, **and** `session-delivered`, **and**
  `session-delivery-acked … disposition=started` (the agent received it and
  started a run), with `delivered=9 failed=0` — the log was read a moment too
  early. Fix: also require one `session-delivered` trace per enqueued envelope
  (`>=`, so a duplicate still trips the exactly-once assertion).
- **`double_steer_busy_then_idle`** — waited on c2's *first* record, then
  `pos()`-unwraped c1's `:s`/`:a`. Two races: `settle_after_ms` can end the run
  before *either* injection is processed (so c1 may legitimately bind as a
  header-marked `:p` — an ambiguity the test already granted c2), and
  `inject_interrupt` writes `:s` and `:a` as two flushed records, so `:s` can be
  logged before `:a` lands. Fix: wait for the **complete** binding of both
  injections (`:s` ⇒ also `:a`, else `:p`) and branch on the binding each
  actually got. Framing invariant untouched — the single-writer funnel still
  guarantees no other envelope interleaves between them.
- **`crash_mid_task_preserves_queue_and_auto_restart_recovers`** — asserted
  `rec.status == Idle` after the chain, but the *last* delivery (`t4`) starts a
  run of its own, so the status at that instant is legitimately `idle` **or**
  `busy`. Fix: wait on the real invariant — `restart_count` is only ever written
  back to 0 *at* a boundary, so `incarnation == 1 && restart_count == 0` is the
  evidence a settle was reached — then accept `Idle | Busy` and keep asserting
  it is never `crashed`/`starting`.
- **verification:** 0 failures in 120 parallel runs (two independent 60-run
  samples) versus 4/30 at the QA commit. The substantive assertions
  (exactly-once, strict FIFO, framing, correlation resolvability, at-most-once
  across a crash, budget reset) are unchanged.

### QA05-06 (HARNESS — found and fixed by QA at the re-gate) — e2e T2 false pass + residual wire-log evidence races

Filed against the fix commits `6d30ef1`/`2518b64`; harness-only (QA-owned
files), fixed by QA in the re-gate commit — no product code involved.

- **(a) e2e-03 T2 was a FALSE PASS.** `scripts/e2e-03-adversarial.sh` T2
  ran `cargo test … repro_qa05_02 -- --exact`: `--exact` demands the FULL
  test name, so the prefix filter matched **zero** tests (`0 passed; … 12
  filtered out`) and cargo exited 0 — the "gating" 10-sample regression
  check executed nothing (ironically the failure mode the fix commit's own
  message called out for `--ignored`). Repro: run the command above and
  observe `0 passed`. Expected: 10 real samples of the race window.
  Fixed: full test name + per-sample proof-of-run (`1 passed` must appear)
  + an explicit harness-broken branch that fails loudly on a zero-match.
- **(b) `session-delivered` is NOT agent-side evidence.** The trace fires
  when the daemon's stdin write RETURNS (kernel-buffered); the FakeAgent
  logs the line only after its reader thread is scheduled — under 4×
  parallel load the log snapshot could still miss the final envelope.
  Measured at the re-gate: **15/80 failures** across four scenarios
  (`fifo_100` last prompt missing; `interrupt_storm` m-9 missing despite
  the QA05-05 delivered-count predicate; `crash_mid_task` tail slice
  off-by-one (`ids[len-4..]` assumed a complete log); `daemon_death`
  replayed-tail snapshot one envelope short). Fixed with agent-side
  evidence: FakeAgent logs a line (flushed) strictly BEFORE answering it,
  so the LAST expected wire id present in the log proves the whole log is
  complete — predicates now wait on that (plus interrupt-resolution
  traces, which are themselves answer-proofs), and the tail assertion is
  position-based. After the fix: **0/170** under the same 4×/3× load.
- Product behavior was correct in every captured failure (delivery traces
  + acks proved receipt); these were read-too-early races in the harness.

## Done

- **04-runtime-engineer (follow-up — QA05-02 + QA05-03 fixed):** telemetry
  tail loss at session exit (branch `sprint/03-agent-engine`).
  - **QA05-02 (was blocking; gate 03/05 FAIL).** New `drain_tail` in
    `supervise.rs`, called from the `Step::Exit` arm before `classify`: after
    the process exits, stdout is read to EOF folding every frame through
    `metrics.observe` + core's pure `SessionStatus::on_agent_event` (no
    duplication of the live loop's reducer), and the tail resets the restart
    budget if it reaches a boundary exactly as the live path would. Covers the
    stop race **and** the crash path — both take that arm, and the frames
    describing a death are the most forensic tail. Two deliberate limits:
    bounded by one `stop_grace` window (normally the write end died with the
    process and buffered lines read back instantly; the budget only matters
    when a `podman exec` intermediate or an orphaned grandchild still holds
    stdout open, which must not wedge the life task) — exceeding it traces
    `session-stream-tail-truncated` rather than losing frames silently; and
    **no delivery in the tail**, because stdin is gone and popping an envelope
    only to fail the write would lose it (at-most-once, `crate::delivery`).
    `Step::Eof` needed no change (reaching EOF means the stream was already
    fully read); `GraceTimeout` is intentionally NOT drained — that arm fires
    only after the grace window was already exceeded, so adding another would
    double the worst case of a path whose purpose is a forced kill.
  - **QA05-03 (nit).** `SessionMetrics::finish` distinguishes
    `JoinError::is_cancelled()` (normal teardown after `abandon()`/kill -9 →
    silent default report, no failure claimed) from a genuine panic (prints,
    counts as `failures: 1`).
  - **Tests.** `repro_qa05_02_stop_midrun_loses_telemetry_tail` un-ignored per
    QA's instruction and re-documented as a regression test — 25/25 green
    locally (it failed 6/15 before the fix). `e2e-03-adversarial.sh` T2 no
    longer invokes it with `--ignored` (which would now match zero tests and
    report a false pass): it samples the race window 10× and **gates**. Added
    `crash_mid_flood_accounts_every_frame_and_closes_the_span` pinning
    crash-path arithmetic (frame budget 1000 → exactly 996 tokens in/out and
    996 micro-USD, `dropped=0`, plus the `agent-task` spike from the span open
    at death) — with an honest caveat in its doc comment that it passes with or
    without the fix (a flood that size reaches EOF first), so it guards the
    crash-path arithmetic, not the race.
  - Verified: `./scripts/e2e-03-adversarial.sh` → PASS (T1 12/12, T2 10/10,
    T3 secrets ratchets); `cargo test -p hamlet-daemon` 3× → 194 green each
    (serial 3.5 s, parallel 1.05 s); `./scripts/ci-local.sh` → `CI battery:
    PASS`.
  - **QA05-04 (second product bug, found by chasing the residual flake;
    decision recorded as ADR-0018).** The
    sink retried its coalesced accumulator only when the *next frame* arrived,
    so a stream that went quiet at a task boundary left its final totals
    unpublished until the life ended: `metrics_for` served `4454` of `4500`
    tokens for a live, idle session, and `stop_session` made the missing tail
    appear instantly (`finish()` → blocking flush; `dropped=0`, so nothing was
    lost, only invisible). That breaks the *live* query surface this step
    exists to provide. Fixed with public `has_pending()`/`retry_flush()` plus a
    `Step::FlushTick` arm armed at `metrics::FLUSH_RETRY_INTERVAL` (100 ms) and
    **guarded by `if metrics.has_pending()`**, so an idle session pays nothing;
    the arm re-pins after firing so it cannot spin without yielding.
  - **QA05-05 (test-side, confirmed against daemon evidence before touching any
    test).** `interrupt_storm` settled on `queue_len() == 0`, which
    `QueueStore::pop` makes true *before* the stdin write — the "missing" mail
    had both `session-delivered` and `session-delivery-acked
    disposition=started` traces, so the log was read too early, not the mail
    lost. `double_steer` waited on c2's first record while c1's binding was
    equally status-dependent and its `:a` could still be in flight.
    `crash_mid_task` asserted a bare `Idle` that the final delivery's own run
    legitimately makes `Busy`. All three settle predicates strengthened;
    **no assertion weakened or removed.**
  - **New guards:** `pending_state_is_visible_and_retry_flush_publishes_it`
    (deterministic mechanism), `live_totals_are_exact_at_a_boundary_without_stopping`
    (live-and-idle exactness, no stop), and `e2e-03-adversarial.sh` **T4** — a
    source ratchet verified to exit 1 with the guard disabled.
  - Re-verified after QA05-04/05: adversarial suite **0 failures in 120
    parallel runs** (two independent 60-run samples) versus 4/30 at the QA
    commit, 6/70 with only `drain_tail`, 1/40 with only the flush tick;
    `cargo test -p hamlet-daemon` 3× → **196 green** each;
    `./scripts/e2e-03-adversarial.sh` → PASS (T1 12/12, T2 10/10, T3 secrets,
    T4 wiring); `./scripts/ci-local.sh` → `CI battery: PASS`.

- **05-qa-engineer:** mock-agent adversarial suite + e2e harness (branch
  `sprint/03-agent-engine`). No product code changed.
  - **New `crates/hamlet-daemon/tests/adversarial_agent.rs`** — 11
    committed FakeAgent scenarios (~1 s total, no podman) + 1 `#[ignore]`d
    bug repro, mapped to the 7 prompt targets:
    1. **Ordering** — `fifo_100_through_a_chatty_slow_agent`: 100 mails
       FIFO through a flooded+garbage-interleaved, held-boundary agent;
       exactly one prompt per boundary; `frames > 2000`, `malformed ≥ 100`.
    2. **Interrupt races** — `interrupt_storm_...` (6 concurrent + 3 late
       steers over an 8-mail burst: framing intact, every wire id belongs
       to an allocated correlation, every interrupt resolved in traces);
       `double_steer_busy_then_idle` (single-writer order s1<a1<s2/a2,
       both bindings accepted); `steer_during_crash_restart_window_...`
       (typed refusals only, chain never cancelled → terminal crash).
    3. **Crash paths** — `crash_mid_task_preserves_queue_and_auto_restart_recovers`
       (queue preserved, at-most-once, stable correlations, TITLE.md NOT
       permanently stamped, restart lineage traced);
       `restart_exhaustion_is_terminal_with_no_zombie_respawn` (700 ms
       observation window: spawn count + charter record frozen).
    4. **Restart recovery** — `daemon_death_mid_delivery_never_replays_an_acked_message`
       (kill -9 after a delivery-ack → recover → tail replayed, acked
       envelope never redelivered).
    5. **Malformed streams** — `malformed_and_smuggling_frames_are_never_applied`
       (garbage, truncated JSON, charter-smuggling + fake-metric records,
       forged readiness/ack responses → skipped/traced, exact totals,
       charter state untouched, no forged ack, no content in any trace);
       `hostile_huge_frames_are_skipped_not_applied` (QA05-01).
    6. **Secrets hygiene** — `secrets_never_reach_files_charter_or_sled`
       (all 4 secret channels populated; byte-scan of EVERY file under the
       tempdir — tree, charter, queue, TITLE.md, sled db, agent log, script
       — plus every trace detail: no secret value, no `API_KEY` shape).
    7. **Backpressure** — `overflow_bounces_and_evidence_caps_hold_under_flood`
       (typed bounce at depth, refused item never persisted; flood →
       ≤8 malformed traces + suppression rollup, exactly 32 unknown-type
       traces, `dropped=0`, exact tokens).
  - **FakeAgent extension (additive; existing scripts byte-identical,
    `deny_unknown_fields` kept):** `crash_if_argv_lacks: "<token>"` disarms
    both crash rules when the token IS in argv — restarted incarnations
    carry `--continue` (ADR-0015 cl. 4), so a script can crash only the
    first life and let the auto-restart survive to drain the queue
    (needed by target 3; the daemon owns stdin so nothing else can
    distinguish lives).
  - **New `scripts/e2e-03-adversarial.sh`** (wired into `ci.sh` via the
    `e2e-*.sh` glob): T1 adversarial suite, T2 QA05-02 regression — GATING,
    10 real samples of the race window with per-sample proof-of-run (was
    informational while the bug was live; briefly a false pass after the
    fix commit, see QA05-06a), T3 secrets ratchets (`.expose()`
    confined to `supervise.rs::session_env`; FakeAgent env-check logs key
    names only). The step-04 follow-up added T4 (flush-tick wiring
    ratchet, ADR-0018).
  - Two harness races tightened while building the suite (test-side, not
    product): committed tests now wait for full stream parse before
    stopping (the mid-run stop window is the QA05-02 repro's job), and
    double-steer assertions accept BOTH legal bindings (a steer's own
    abort can settle the run before the next steer is injected).
  - Verified: `cargo test -p hamlet-daemon --test adversarial_agent` →
    11 passed / 1 ignored, 10 consecutive runs green; repro fires 6/15;
    `./scripts/ci-local.sh` → `CI battery: PASS`.

- **04-runtime-engineer:** daemon-parsed stream telemetry → sled (branch
  `sprint/03-agent-engine`; ADR-0016 + ADR-0017).
  - **New module `crates/hamlet-daemon/src/metrics.rs`** (~1000 lines incl.
    26 golden-vector unit tests), split so telemetry can never starve the
    step-03 single-writer funnel:
    - `StreamExtractor` — a **pure fold** (`&AgentEvent` + caller-supplied
      `at_unix_ms` → `Observation`s). No I/O, no clock (CONVENTIONS §3 r4),
      no allocation on the hot path (the caller owns the scratch `Vec`). It
      runs inline in the life task's stdout arm *before* any supervision
      branch, so pre-readiness and malformed frames are measured too and
      nothing an agent self-reports is ever believed (SPEC §6).
    - `SessionMetrics` + `spawn()` — the per-session **sink task** owning
      every sled write, fed by a bounded (1024) channel with `try_send`
      only. One wake folds the whole queue into a single `add_tokens` +
      `add_cost_micro_usd` plus the append-only records, so a 10k-frame
      stream collapses into a handful of sled writes.
    - **Backpressure policy (ADR-0017 cl. 2):** additive observations
      (tokens, cost) coalesce into a local accumulator and are retried —
      accounting is *lossless* however far the sink falls behind;
      append-only records (spikes, traces) queue to 64, then the oldest is
      dropped and the loss counted. Both counters surface in the summary.
  - **Token/cost accounting (ADR-0016 cl. 2–3).** pi's
    `message_update.usage` is *cumulative* per assistant response, so the
    extractor differences snapshots against a per-message baseline instead
    of summing them (a naive sum multiplies the total by the frame count —
    there is an explicit regression test for that). `message_end.message
    .usage` is authoritative and closes the message; a regressed
    `totalTokens` rebases the baseline so counters can never underflow.
    Only `input`/`output` reach `tokens.v1` (the Sprint-01 `TokenTally`
    shape is fixed, ADR-0005 cl. 5); cache/reasoning/`totalTokens` are
    counted in `StreamStats` and reported in the summary trace. Cost →
    integer micro-USD (`cost_micro_usd`: half away from zero, `cost.total`
    when finite and positive else the component sum, NaN/negative → 0),
    differenced *after* conversion so no float drift accumulates.
  - **Latency-spike definition (the prompt's recorded decision, ADR-0016
    cl. 1).** A spike is a *stream span measured between frame reads*
    whose duration is `>= sessions.latency_spike_ms` — a **new config key**
    (v3 → **v4**, default **30 000 ms**, range 1..=3 600 000, additive with
    a serde default so v1–v3 trees still parse; core + CLI goldens updated
    in-commit). Closed operation vocabulary: `agent-run`
    (agent_start→agent_end), `agent-task` (first agent_start→agent_settled,
    spanning continuation runs), `agent-turn` (turn_start→turn_end),
    `tool-call <name>` (tool_execution_start→_end matched by `toolCallId`),
    `stream-gap` (a frame gap **while a task is in flight and no tool span
    is open** — idle time never spikes, and a slow tool is attributed to
    its own span only, never double-reported), `delivery-ack` /
    `interrupt-ack` (a wire id this daemon wrote → its `response`, joined on
    the step-03 `<corr>:{q,s,a,p}` ids via the new `PiCommand::id()`).
    Spans still open at stream end are closed and evaluated by `finish()`,
    so a session killed mid-task still records the span it died in.
  - **Traces (ADR-0017 cl. 4 — one source per event).** Lifecycle traces
    stay in `supervise`; `metrics` owns the stream vocabulary:
    `session-delivery-acked`, `session-interrupt-acked`,
    `session-tool-call`, `session-agent-retry`, `session-stream-event`
    (compaction/auto-retry), `session-stream-malformed` (+
    `-suppressed`), `session-stream-unknown-record` (the pi-version-drift
    signal: first sighting of each unmodelled type), and one
    `session-stream-summary` rollup per life. Acks are traced on **success**
    only — a failed `response` is still traced by the writer side, which
    owns the drain gate (`session-delivery-rejected`), so no event has two
    sources. Malformed frames are counted and skipped, never fatal, and
    their **content is never recorded** (a frame may echo mail text or a
    secret — CONVENTIONS §6): reason + byte length only.
  - **Bounded evidence (ADR-0017 cl. 3).** Per life: 8 malformed traces,
    64 tool markers, 32 distinct unknown record types, 256 open tool spans,
    64 pending acks — all FIFO-evicted/counted, with the uncapped totals
    preserved in `session-stream-summary`, so flooding the stream can
    neither grow memory nor hide evidence. Agent-supplied strings pass
    `supervise::scrub_detail` and a 64-char cap.
  - **Live query surface (ADR-0017 cl. 5).**
    `Telemetry::metrics_for(MetricsScope::{Session, Agent, Commission, Uid},
    window) -> MetricsSnapshot` — serializable, sorted, clock-free, no new
    sled trees and no key-layout change (the ADR-0005 boundary test still
    passes untouched). `Session` scope is session-exact (no agent rollup:
    traces are agent-keyed); `Uid` crosses commissions and so walks the
    trees, making `Agent` the preferred scope when the commission is known.
    `Supervisor::metrics_for_{session,uid,commission}` resolve the
    commission from the charter slot — this is what CLI (08) and WS (09)
    call.
  - **Supervision wiring.** `run_life` is now a thin wrapper that spawns
    the sink, calls `run_life_inner`, and **guarantees `finish()` on every
    exit path** (crash, kill, EOF, readiness failure) before returning the
    `LifeEnd`. The write side gained a `DrainGate { session, status, ready,
    draining }` value object so a queue drain and an interrupt injection
    cannot evaluate the gate differently (behaviour unchanged; the 24
    step-02/03 tests pass untouched). Sink write failures are traced as
    `session-telemetry-failed`, never propagated.
  - **FakeAgent metric streams (additive; existing scripts byte-identical,
    `deny_unknown_fields` kept):** `usage_sequence` (cumulative snapshots
    per turn), `turns_per_run`, `emit_message_end` (authoritative final
    usage), `tool_calls[{name,delay_ms,is_error}]`, `flood_frames` +
    `flood_usage` (soak: cumulative `step × i`, so totals are exactly
    `step × N` per turn), `malformed_every_n` (garbage interleaved *by
    script* — the daemon owns stdin, so a test cannot cue mid-run),
    `unknown_records`, and `crash_after_frames` (die mid-run, so a task
    span is open when the process goes; `crash_after_commands` fires before
    the run starts and cannot model that).
  - **Tests.** `tests/metrics.rs` — 9 FakeAgent scenarios (~1 s): exact
    token/cost totals from a scripted two-turn stream (and a charter.json
    assertion that no telemetry leaked into it), `metrics_for` across all
    three scopes, latency spikes following the config threshold (same
    stream: spikes at 50 ms, silent at 1 h), malformed garbage interleaved
    through a real run (session still settles, totals exact, no restart,
    content never in any trace), unknown record types as the drift signal,
    **10k-frame soak** (a mid-flood `steer()` returns while the stream is
    still `busy` = the delivery writer kept its turn; the queued mail is
    delivered afterwards; the summary's own counters must equal what landed
    in sled, `dropped=0`, and `tokens == frames-step × runs`), interrupt
    acks joining on the correlation (all three critical-interrupt records
    `q`/`s`/`a`), kill-9 restart resumability (metrics survive `abandon()`
    + `recover()`, and the restarted incarnation accumulates under its own
    session key while the agent rollup grows), and crash-mid-task still
    recording `agent-task` + a summary. Plus 26 `metrics.rs` lib goldens
    (cost conversion incl. NaN/negative/saturation, cumulative
    differencing, the naive-summing regression guard, `message_end`
    non-double-counting, usage regression rebasing, inclusive threshold
    boundary, every span family, `stream-gap` mid-task only, `finish()`
    closing open spans, no-content-leak + caps, unknown-type counting,
    hostile-string scrubbing, ack joining, forged/foreign response ids,
    span and ack bounds, summary rendering, and lossless coalescing under a
    capacity-1 channel) and 6 `telemetry.rs` snapshot tests.
  - Verified: `cargo test -p hamlet-daemon` (110 lib + 9 metrics + 14
    supervise + 10 delivery) and `./scripts/ci-local.sh` → `CI battery:
    PASS`.

- **03-runtime-engineer:** sequential delivery queue + Divine Interruption
  (branch `sprint/03-agent-engine`).
  - **Queue persistence — recorded location decision:** undelivered queues
    live at **`courier/queues/<uid>.jsonl`** (courier = the communication
    plane, host-side, survives parcel moves/hibernation; per-uid typed `u32`
    file name — no sanitizer surface). New typed accessors
    `HamletTree::courier_queues()` / `HamletTree::session_queue(uid)`
    (ADR-0003 accessor-only policy; lazy dir, golden-tested). Encoding: one
    canonical `SessionEnvelope::to_json_line()` record per line — the
    step-01 envelope schema is also the persistence schema (correlations
    are stable across restarts).
  - **New module `crates/hamlet-daemon/src/delivery.rs`:** `QueueStore`
    (write-through cache over the file, loaded lazily per uid; the daemon
    is the only writer so the cache cannot diverge) + typed `QueueError
    {Overflow{uid,depth}, Corrupt{uid,line,reason}, Session, Io}`.
    Persistence discipline: atomic whole-file rewrite (unique temp in-dir →
    write → fsync → **mode 0600** (mail text may be sensitive) → rename →
    dir fsync; ADR-0006 pattern); an empty queue REMOVES its file (no tree
    litter); corruption is a hard typed error carrying the line number,
    never the line content (CONVENTIONS §6). **At-most-once:** `pop`
    persists the removal BEFORE the stdin write — a crash in between loses
    that one envelope (traced) rather than replaying it after restart.
  - **Supervisor API:** `enqueue(uid, kind, text) -> CorrelationId`
    (sync; mail|tasking|notice; works offline — persisted, delivered after
    the next `start_session` — and live; correlation seq from sled
    `counters.v1` domain 3; pokes the life task via `Control::Deliver`)
    and `steer(uid, source, priority, text) -> CorrelationId` (async;
    **the daemon API** for the Divine Interruption — CLI `hamlet agent
    steer`/HTTP surface is Sprint 08 polish over it, Sprint 05 mayor
    decrees call the same with `source: mayor`; no axum server exists yet,
    `web.rs` is a Sprint 04 stub). Backpressure: bound =
    `sessions.queue_depth` (config v3, default 64); overflow →
    `SuperviseError::Queue(QueueError::Overflow{uid, depth})` +
    `session-queue-overflow` trace — **this typed error is the bounce
    hook** Sprint 04's mail router routes back to the sender.
  - **Race safety (single-writer funnel extended):** the step-02 `Control`
    mpsc gained `Deliver` (wake-poke; the queue itself lives in the
    file-as-truth store, not the channel) and `Interrupt{envelope,
    reply: oneshot}`. Every stdin byte after the readiness probe is
    written by the life task's select loop — a drain write and an
    interrupt injection can never interleave records; interrupts never
    queue behind pending deliveries because at most ONE envelope is ever
    in flight. Drain gate: one envelope per task boundary
    (`running`/`idle` per the parsed stream), `prompt_in_flight` released
    only by `agent_start`, `agent_settled`, or a rejected `:p` response
    (pi answers mid-stream prompts with success=false — traced
    `session-delivery-rejected`, envelope lost per at-most-once).
  - **Interrupt semantics (ADR-0015 cl. 3 via `pi_binding`):** busy →
    `[clear_queue(:q)?critical] + steer(:s) + abort(:a)`; running/idle →
    single header-marked prompt (`:p`, `[hamlet-interrupt source=…
    priority=… corr=…]` — gate the queue like any run); starting →
    `NotAtTaskBoundary`; stop-draining → typed
    `SuperviseError::Draining(uid)`; during restart backoff →
    `SessionError::NotAlive(Crashed)`. The backoff select now
    distinguishes stop/kill (cancels the restart, as before) from
    Deliver (stays queued for the next life) and Interrupt (typed refusal,
    restart unaffected). Correlation ties the interrupt to the stream:
    every wire record id is `<corr>:{q,s,a,p}` (step 04's telemetry
    consumes the responses). Traces: `session-interrupt-injected`,
    `session-interrupt-refused`.
  - **Brief path superseded (step-02 flag resolved):** `start_session`
    enqueues the brief as the first tasking envelope — delivered at the
    first readiness drain; it now SURVIVES a pre-readiness crash (stays
    queued) and is never replayed once written. `deliver_brief_once`
    removed; `session-brief-*` traces replaced by `session-enqueued` /
    `session-delivered` (+ `session-delivery-failed` /
    `session-delivery-rejected` / `session-queue-overflow`; module rustdoc
    vocabulary updated — step-02 test goldens updated in-commit).
  - **Stop semantics:** graceful stop / hard kill do NOT drain the queue —
    undelivered envelopes stay persisted for the next session; the
    remaining count is traced in `session-stop-requested`.
  - **FakeAgent extension (additive, schema still `deny_unknown_fields`):**
    a reader thread now feeds the command loop — held runs never block
    stdin, so interrupts/cues are processed mid-task exactly like real pi;
    new script field **`settle_after_ms`** auto-releases a held settle
    after N ms (the daemon owns stdin, so timed tests cannot send the
    `settle` cue). Existing scripts/scenarios unaffected (14 supervise
    tests green untouched).
  - **Tests:** new `tests/delivery.rs` — 10 FakeAgent scenarios (~1.5s):
    FIFO ordering under load (5 offline-enqueued mails; probe first, then
    exact wire-id order; queue file removed when empty), boundary
    semantics (held run: ZERO mid-task writes, exactly one prompt per
    settle), enqueue-poke delivers to an idle session without a boundary
    event, interrupt mid-task with correlation (exact steer+abort wire
    bytes via the shared binding, header content, queue preserved and
    drained after), critical interrupt (`clear_queue` order q<s<a<mail;
    hamlet-side queue untouched), interrupt at idle (header-marked prompt
    golden bytes), typed refusals (not-live / backoff-NotAlive /
    empty-text / after-stop), overflow bounce (typed error, refused item
    never persisted, trace present, NoSlot for unknown uids),
    **restart-recovers-queue** (`abandon()` = kill -9 → fresh Supervisor →
    `recover()` heals → operator restart → incarnation 1 delivers the
    recovered envelopes in order with STABLE correlations; queue file
    asserted on disk mid-crash), interrupt-during-drain race (6-mail burst
    + 3 interrupts while boundaries cycle at 60ms: every logged stdin
    line parses as exactly one pi command = no framing corruption; each
    mail delivered exactly once, FIFO). Plus 6 `delivery.rs` lib tests
    (FIFO+depth, overflow typed+not-enqueued, survives store replacement
    with 0600 + no temp litter, corrupt → typed line-number error with NO
    content leak, unknown uid = empty, interrupt envelopes round-trip
    losslessly). Core: tree golden-path assertions for the new accessors.
  - Verified: `cargo test -p hamlet-daemon --test delivery` (10) +
    `--test supervise` (14) + `cargo test --workspace` green;
    `scripts/ci-local.sh` → `CI battery: PASS` (fmt, clippy -D warnings,
    workspace tests, check-deps, e2e).

- **02-runtime-engineer:** session spawn & supervision (branch
  `sprint/03-agent-engine`).
  - **Deliverable 1 — FakeAgent built** at the ADR-0015 cl. 5 recommended
    location (final call): `crates/hamlet-daemon/src/bin/fakeagent.rs` —
    second binary target of the daemon crate, no new workspace member.
    Speaks exactly the pi wire dialect: reads via
    `session::parse_pi_command` (unknown → pi-style `parse` failure
    response), emits records `parse_agent_event` accepts, strict LF framing
    (std `Lines`: splits on LF, strips one trailing CR; never U+2028/9),
    stderr diagnostics only, flushed per record. Default behavior = the
    contract's minimal emulation (prompt → started + agent_start/turn/
    message_update(usage)/agent_end/agent_settled; steer → queued +
    continuation runs; abort → end run, continue queued steering, respond
    only once settled; clear_queue → dropped texts; get_state →
    isStreaming; EOF → exit 0). **Script schema (owned by this step;**
    `--script <path>` argv or `FAKEAGENT_SCRIPT` env, `deny_unknown_fields`):
    `log` (append every received stdin line), `event_delay_ms`,
    `never_ready` (suppress probe responses), `slow_task` (hold settle),
    `ignore_eof` (survive stdin close — grace-window tests),
    `crash_exit_code`, `crash_after_commands`, `malformed` (raw lines),
    `usage`, `expect_env` (startup check, result logged as
    `env-check ok|mismatch|missing:<KEY>` — values never logged).
    **Cues** (`{"type":"fake_cue","cue":…}`, never echoed): `crash`,
    `hang` (freeze: read+log, answer nothing), `settle` (release held
    slow_task), `malformed`. Deterministic: content/order derive solely
    from script + received command sequence.
  - **Deliverable 2 — `hamlet-daemon::supervise`** (new, ~1150 lines +
    golden unit tests): `Supervisor::start_session(parcel, brief, secrets)`
    / `start_session_with_cmd` → argv per ADR-0015 cl. 1 through the new
    pure builder `podman::session_exec_argv` (`exec -i --user <uid>:<uid>
    --workdir /home/<uid> [-e K=V sorted]… <container> <cmd…>`, typed
    workdir via `home_container_dir`, uid/container/env/cmd validated —
    golden-tested incl. redaction). `Runtime` grew `spawn_session(spec)`
    (both impls; `FakeRuntime` records redacted argv + failure injection);
    `ExecHandle` was redesigned for concurrent supervision: `take_pipes()`
    (detached `ExecPipes{stdin,stdout,stderr}` — borrowing accessors cannot
    be split across tasks), `kill()`, `wait()`; shared substrate
    `podman::spawn_child_exec` (piped, `kill_on_drop`, argv redacted before
    any error path).
  - **Architecture:** one tokio task per life *chain*; inside a life the
    pipes split three ways — stdout line reader → `parse_agent_event`
    (never fatal) → status reducer → charter; stdin owned solely by the
    task (single-writer invariant preserved for step 03); child owned by a
    reaper task (poll_fn shape: ends the `wait` borrow before `kill` —
    documented in-code). Charter transitions go through the closed table
    (`SessionStatus::transition`) + incarnation-monotonicity check inside
    `update_charter` (lock → load → mutate → validate → `save_atomic`).
    Health: readiness = `get_state` probe (`rdy-<uid>.<inc>`) within
    `ready_timeout_ms` → Starting→Running; `agent_start`→Busy,
    `agent_settled`→Idle + `restart_count` reset (first boundary of a
    life). Crash paths: unexpected exit / readiness timeout / readiness
    refused / spawn failure → Crashed + sled trace → pure backoff
    (`RestartConfig::backoff_ms` over the persisted count) → new
    incarnation (`next_incarnation`, `--continue` when
    `continue_on_restart`); exhaustion → permanently crashed + TITLE.md
    lineage stamp (`title_crash_line`, ADR-0007 append discipline:
    existing regular file only, never create/symlink-follow) +
    `session-permanently-crashed` trace. Trace vocabulary documented in
    the module rustdoc (18 events, `session-*`).
  - **Stop/kill APIs for Sprint 07:** `stop_session` (graceful: close
    stdin = pi's EOF shutdown signal, `stop_grace_ms` window, then hard
    kill; returns final charter status), `kill_session` (immediate SIGKILL
    semantics → `stopped`, never `crashed`), `shutdown()` (all live uids,
    sorted), `abandon()` (test/shutdown seam: kill children + abort tasks
    WITHOUT charter writes — the `kill -9` state), `recover()` (CONVENTIONS
    §3 r6: every charter-alive session without a live task → `crashed` +
    trace; refuses while tasks are live). `start_session` self-heals a
    stale alive record the same way.
  - **Secrets (SPEC §11.4):** `session_env(structural, secrets)` (pure,
    golden-tested) — `HAMLET_API_KEY_<P>` → `<P>_API_KEY` (uppercased),
    `PI_TELEMETRY=0`, `PI_SKIP_VERSION_CHECK=1`; values reach the exec's
    `-e` env ONLY (never files in home; every log/error path goes through
    `redacted_argv`; end-to-end test asserts the values arrive in the
    spawned agent AND that nothing value-shaped reaches logs).
    **`HAMLET_MODEL_ENDPOINT` decision (step-01 open issue, resolved):**
    pi consumes custom endpoints via `models.json` in
    `PI_CODING_AGENT_DIR` (pi docs `models.md`/`custom-provider.md`), which
    needs an api-type + model catalog the `.env` surface does not carry —
    so endpoint/key pass through as exec env (`HAMLET_MODEL_ENDPOINT`,
    `HAMLET_MODEL_API_KEY`) and `models.json` rendering is DEFERRED to a
    provisioning-side step; when rendered, its `apiKey` must use
    `$HAMLET_MODEL_API_KEY` env interpolation so the secret still never
    touches a file.
  - **Brief handling:** delivered once per chain (first life only) as a
    tasking envelope at the Running boundary via `pi_binding`, correlation
    `t-<seq>-<uid>.<inc>` with seq from sled — new
    `Telemetry::next_correlation_seq` (additive `counters.v1` domain 3,
    ADR-0005 cl. 3 layout untouched). Restarted incarnations rely on pi
    `--continue` instead. Step 03's queue supersedes this direct path.
  - **Test seam:** `SessionSpawner` trait — `PodmanSpawner` (delegates to
    `Runtime`, production) and `HostProcessSpawner` (runs the FakeAgent
    host binary with the spec env — no podman). 14 integration scenarios
    (`tests/supervise.rs`, all green in ~1.1s): clean run (exact wire-byte
    goldens for probe + brief prompt from the script log), crash-restart
    (incarnation bump, brief not re-delivered, crash/restart traces),
    restart-limit exhaustion (TITLE.md stamp + 3 crash traces +
    permanent trace), readiness timeout (max_restarts=0), stop-during-
    backoff (stays crashed — crashed→stopped is not a legal transition;
    recorded decision), graceful stop at boundary, grace-window kill
    (`ignore_eof` + 500ms grace, `session-grace-expired`), hard kill
    mid-task, daemon-death recovery (`abandon` → stale alive charter →
    `recover` → crashed → operator restart = new incarnation), secrets
    env end-to-end, podman argv template + redaction (FakeRuntime),
    `--continue` on restarted argv, parcel_ref/NoSlot/NotLive typed
    errors, AlreadyActive refusal, multi-session shutdown.
  - **Guarded live smoke** (`tests/supervise_guarded.rs`,
    `podman_live_session_spawn_message_stop`): real `PodmanRuntime` +
    `PodmanSpawner`, spawn → readiness → (message) → graceful stop, then
    asserts the CONTAINER outlives the session and no crash traces. Agent
    choice is probed at runtime: real pi (`pi --mode rpc`, readiness-only —
    no credentials for a tasking run) if `command -v pi` succeeds, else a
    **`sh` echo session** (minimal pi-dialect JSONL script written to the
    parcel home via `tee`) — the default blueprint base image is plain
    alpine without pi, so the echo fallback is the expected CI path.
    NOT EXECUTED on the dev host (no podman on PATH — skip path verified);
    runs in the podman-capable CI job (name matches the `--ignored podman`
    filter).
  - **Gotchas found (bind steps 03/07):** (1) tokio's
    `ChildStdin::poll_shutdown` is a NO-OP — stdin EOF requires DROPPING
    the write end (supervise.rs does `drop(stdin.take())`); (2) a
    `select!` arm on an already-elapsed timer stays ready and spins
    WITHOUT yielding — on a current-thread runtime that starves sibling
    tasks (the reaper); one-shot deadlines must exit the loop or be
    re-armed (GraceTimeout now kills + bounded-await + returns).
  - Verified: `cargo test -p hamlet-daemon` → lib 72 (+10 new: session
    argv goldens/rejections/kill/redaction, session_env, pi_session_cmd,
    title line, scrub) + supervise 14 + all prior suites green;
    `scripts/ci-local.sh` → `CI battery: PASS` (fmt, clippy -D warnings,
    workspace tests, check-deps, e2e).

- **01-architect:** session protocol + `interrupt_and_steer` wire schema
  (branch `sprint/03-agent-engine`).
  - **ADR renumbering (binding for steps 02–07):** this sprint's prompts
    cite "ADR-0008", but 0008 was claimed by Sprint 01 (slot identity
    uniqueness) and ADR numbers are never reused (docs/adr/README.md). The
    decision is recorded as **ADR-0015**
    (`docs/adr/0015-session-protocol.md`); README index + renumbering table
    updated (note: Sprint 09's cited "ADR-0015" is now also taken → 0016+).
    Wherever a later prompt says "ADR-0008", read **ADR-0015**.
  - **pi protocol evidence (recorded in ADR-0015 Context):** inspected pi
    0.99.1 on the dev host — `pi --help` (`--mode rpc`), docs `rpc.md`,
    `rpc-commands.md`, `json.md`. Findings that shaped the schema:
    strict JSONL framing (one JSON object per LF; strip one CR; U+2028/9
    are NOT boundaries — don't use line readers that split on them);
    stdin commands `prompt`/`steer`/`abort`/`clear_queue`/`get_state`;
    stdout `response` + session events; **no native interrupt-and-steer**
    (`steer` only queues; `abort` interrupts but *continues* queued
    messages — that composition IS the Divine Interruption); RPC mode
    emits **no startup banner** (readiness must be probed via `get_state`);
    `agent_end` is NOT a task boundary (retries/steering may follow) —
    `agent_settled` is.
  - `crates/hamlet-core/src/session.rs` (new, ~1000 lines + 20 unit
    tests): `SessionId {uid, incarnation}` (uid ∈ {1000, 1001–2000},
    hard error otherwise; `token()` = `"<uid>.<inc>"`; `as_u64()` =
    `inc<<32|uid` — the ADR-0005 sled `session u64`; `next_incarnation()`
    never wraps); `SessionStatus` 7-state enum (starting/running/busy/
    idle/crashed/stopped/hibernating) with a **closed transition table**
    (`can_transition_to`/`transition`, self-loops allowed; crashed/stopped
    → starting only via a new incarnation), `is_alive`, `accepts_delivery`
    (running|idle), pure stream reducer `on_agent_event` (agent_start →
    Busy; agent_settled → Idle); `SessionRecord {id, status,
    restart_count}` (charter group entry).
  - **Envelopes:** `SessionEnvelope` — internally `kind`-tagged canonical
    JSONL: queued `mail|tasking|notice {correlation, text}` vs
    out-of-band `interrupt_and_steer {correlation, source: system|human|
    mayor, priority: normal|critical, steering_text}` — queued and
    interrupt are distinguishable by tag, correlation prefix (`m/t/n` vs
    `i`), pi command type, AND the rendered `[hamlet-interrupt …]` header
    (SPEC §6). `CorrelationId` (charset `[A-Za-z0-9._-]`, ≤64;
    `<kind>-<seq>-<uid>.<inc>`; `wire_id(suffix)` → `<corr>:{p,s,a,q}`).
    Empty/whitespace-only payloads rejected at construction and at parse.
  - **pi wire binding** (`pi_binding(envelope, status)`, pure + golden-
    tested): queued → single `prompt` at running/idle only (typed
    `NotAtTaskBoundary` otherwise — and pi independently rejects
    mid-stream prompts, so boundary bugs cannot silently interleave);
    interrupt busy → `[clear_queue?critical] + steer(:s) + abort(:a)`;
    interrupt running/idle → single header-marked `prompt`; starting →
    `NotAtTaskBoundary`; crashed/stopped/hibernating → `NotAlive`.
    `readiness_probe(session)` → `get_state` with id `rdy-<uid>.<inc>`.
    `PiCommand` is a closed 5-variant enum with `to_json_line` +
    `parse_pi_command` (the FakeAgent's reader).
  - **Event side:** `parse_agent_event` — never fatal: typed
    `Response{id, command, success, error, disposition}`, `AgentStart`,
    `AgentEnd{will_retry}`, `AgentSettled`, `TurnStart/TurnEnd`,
    `MessageUpdate{usage}` (typed `Usage`/`Cost`, camelCase per pi
    `json.md`); unknown record types → `Other{record_type, raw}`
    (forward-compat); garbage → `Malformed{line, reason}` (step 04:
    trace + skip).
  - **Charter v1→v2 (additive, ADR-0004 policy):** new `sessions` group
    (uid → `SessionRecord`), `CHARTER_VERSION=2`, first real migration
    step (`step_1_to_2`); strict parsing moved to a `StrictCharter` wire
    mirror — unknown fields are corruption at every supported version,
    while v1 files simply omit the new group. Invariants: key == `id.uid`,
    slot must exist. Goldens + migration + lying-charter tests committed.
  - **config.json v2→v3 (additive):** `sessions` section = ADR-0015 cl. 4
    restart-policy keys: `ready_timeout_ms` 30000 (1k–600k),
    `stop_grace_ms` 10000 (500–300k), `queue_depth` 64 (1–10k),
    `continue_on_restart` true, `restart.{max_restarts 5 (0–100),
    backoff_initial_ms 1000, backoff_max_ms 60000}`; optional
    `provider`/`model` (argv-token charset). Backoff =
    `min(initial·2ⁿ, max)`, n = persisted `restart_count` — pure
    (`session::restart_backoff_ms` / `RestartConfig::backoff_ms`),
    exhaustion → `None` = permanently crashed. No wall-clock state
    (CONVENTIONS §3 r4).
  - **FakeAgent spec** (deliverable 3 — specified, NOT built; step 02
    builds): normative contract in the `hamlet-core::session` module
    rustdoc ("FakeAgent contract") + ADR-0015 cl. 5. Summary: reads the
    **pi wire dialect** (the exact JSONL bytes `pi_binding` emits — one
    writer path for fake and real pi), emits pi-shaped events/responses,
    scripted via JSON scenario (tokens/costs/latencies/crash-on-cue/
    slow-task hang/malformed-frame injection), test-only cue records
    `{"type":"fake_cue","cue":"<name>"}`, stderr for diagnostics only,
    fully deterministic. **Recommended location:
    `crates/hamlet-daemon/src/bin/fakeagent.rs`** (second binary target —
    no new workspace crate, ADR-0001 untouched; step 02 records the final
    call).
  - Fixtures updated in-commit: `crates/hamlet-cli/tests/golden/
    {charter.json,config.json}`, `scripts/e2e-01-topology-state.sh`
    (charter_version 2), charter/config inline goldens.
  - Tests: hamlet-core lib now **91** (was 65; +20 session, +3 charter,
    +3 config): golden envelope vectors (all 5 variants, exact JSONL
    bytes), binding goldens (incl. critical-interrupt 3-record sequence),
    7×7 transition-matrix exhaustiveness, event-parse goldens (verbatim
    pi doc examples), never-fatal parser suite, backoff schedule golden,
    correlation charset rejections, charter v1→v2 migration + sessions
    invariants + v2 unknown-field strictness, config v2→v3 migration +
    sessions validation matrix.
  - Verified: `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS`. Commit: `feat(core): session protocol +
    interrupt_and_steer schema (ADR-0015)`.

## Decisions (ADR refs)

- **Step 04 → ADR-0016 + ADR-0017** (the step-03 flag asked for "a small
  ADR or a HANDOFF-recorded decision"; the threshold turned out to be
  coupled to the cumulative-usage semantics and the sink architecture, so
  it is two ADRs): **ADR-0016** = what a metric *is* (latency-spike
  definition + `sessions.latency_spike_ms` config v4, cumulative-usage
  differencing, micro-USD conversion, malformed-frame content policy);
  **ADR-0017** = how it *flows* (pure fold inline + per-session sink task,
  lossless-coalescing backpressure, per-life evidence caps, trace-vocabulary
  split, `metrics_for` query surface).

- **Step 03 recorded decisions (no new ADR — all within ADR-0015 cl. 2–4,
  ADR-0003/0005/0006 patterns):** queue location
  `courier/queues/<uid>.jsonl` (typed accessors; courier plane, host-side,
  survives parcel moves); envelope JSONL = persistence encoding
  (correlations stable across restarts); **at-most-once** delivery (pop
  persists before write — loss traced, never a duplicate replay); overflow
  = typed `QueueError::Overflow` (the Sprint 04 bounce hook) and refused
  items never touch the file; interrupt during restart backoff →
  `NotAlive(Crashed)` refusal (does NOT cancel the restart — only
  stop/kill does); interrupt during stop-drain → `Draining(uid)`;
  graceful stop does not drain the queue (undelivered items persist for
  the next session); brief = first queued tasking (supersedes the
  direct path; `session-brief-*` traces retired); FakeAgent
  `settle_after_ms` + reader thread (additive schema extension — the
  daemon owns stdin, so timed tests need auto-release instead of cues).

- **Step 02 recorded decisions (no new ADR needed; all within ADR-0015 +
  ADR-0005 layout):** FakeAgent at `src/bin/fakeagent.rs` (ADR-0015 cl. 5
  recommendation confirmed); script schema + cue vocabulary (above);
  `ExecHandle` take-based split + `kill` (supervision needs concurrent
  read/write/kill/wait); correlation seqs = `counters.v1` domain 3
  (additive, ADR-0005 cl. 3); endpoint/model-key env passthrough with
  `models.json` deferred (interpolation keeps the secret out of files);
  operator `start_session` resets `restart_count` (fresh budget) while
  auto-restart increments it; stop-during-backoff leaves the session
  `crashed` (transition table forbids crashed→stopped) with the restart
  cancelled + traced; hard kill lands on `stopped` (daemon-intended, not a
  crash); grace-window expiry escalates to kill and still lands `stopped`.

- **ADR-0015** — session protocol: transport (pi `--mode rpc` JSONL over
  `podman exec -i`), argv template (cl. 1), envelope schema (cl. 2),
  binding + interrupt semantics + task boundary + readiness (cl. 3),
  identity/status/charter v2 + restart keys/config v3 (cl. 4),
  FakeAgent dialect (cl. 5). Body is 45 lines vs the ≤40 cap (precedent:
  ADR-0009/0010 at 42–43) — content is normative; QA may rule.
- **Session argv template (ADR-0015 cl. 1):**
  `podman exec -i --user <uid>:<uid> --workdir /home/<uid> [-e K=V]…
  <container> pi --mode rpc [--provider P] [--model M] [--continue]`.
  Note for step 02: Sprint 02's `exec_argv`/`spawn_exec` do NOT yet
  support `--user/--workdir/-e` on `podman exec` — extend the Runtime
  seam with a session-spawn variant (keep pure argv builders + golden
  tests + `redacted_argv` masking, which already handles `-e K=V`).
  Container name is sanitizer-checked and the command is the fixed token
  `pi`, so there is no flag-injection path — keep `validate_cmd` guards.
- **Interrupt = steer-then-abort** (busy): pi's abort *continues* queued
  steering messages (pi docs `rpc-commands.md` `clear_queue`) — that
  composition is the Divine Interruption; critical priority prefixes
  `clear_queue`. Idle/running sessions get a header-marked `prompt`
  instead (nothing to interrupt). If pi rejects a prompt as mid-stream
  (stale status), retry as the steer+abort sequence — the single-writer
  task (step 03) serializes this.
- **Secrets mapping (step 02 consumes):** inject via exec `-e` only
  (never files in home): `Secrets::provider_keys`
  (`HAMLET_API_KEY_<PROVIDER>`) → `<PROVIDER>_API_KEY` env names pi's
  providers read (docs `providers.md`); plus `PI_TELEMETRY=0`,
  `PI_SKIP_VERSION_CHECK=1`. `HAMLET_MODEL_ENDPOINT` → pi custom-provider
  config needs a decision in step 02 (see pi `custom-provider.md`; if it
  requires a settings file in `PI_CODING_AGENT_DIR`, that dir lives in
  the parcel home — record the choice in HANDOFF).
- **Readiness:** send `readiness_probe()` right after spawn; the
  `Response` with id `rdy-<uid>.<inc>` + success performs Starting →
  Running; `sessions.ready_timeout_ms` expiry → Crashed (restart policy).
- **Restart semantics:** auto-restart = new incarnation
  (`SessionId::next_incarnation`), `restart_count` persisted in the
  charter `SessionRecord`, backoff from `RestartConfig::backoff_ms`,
  budget exhaustion → permanently `crashed` + TITLE.md lineage stamp +
  sled trace; counter resets on first `agent_settled`;
  `continue_on_restart` → add pi `--continue` for incarnations > 0.

## Open issues / flags for later steps

- **QA05-02 (RESOLVED — see the FIX block in "QA findings"):** `drain_tail`
  folds stdout to EOF after exit on the shared `Step::Exit` arm (stop and
  crash), bounded by one stop grace and traced if it exceeds it; the repro
  is un-ignored and `e2e-03-adversarial.sh` T2 gates on 10 samples.
  **Remaining work for QA (step 05/06):** re-issue the gate verdict.
  New trace event to fold into any vocabulary list:
  `session-stream-tail-truncated`.
- **QA05-01 (ruled, SHOULD-FIX → Sprint 04):** oversized-frame
  allocation before any size guard (supervise.rs:1663/1705). Behavioral
  spec met (huge frames skipped/traced/never applied — committed test);
  residual DoS surface tracked to Sprint 04: capped incremental reader
  (suggest ~1 MiB → malformed-on-cap, counted in the summary). This
  resolves the step-04 "left for step 05 to judge" flag.
- **QA05-03 (RESOLVED):** `finish` distinguishes `JoinError::is_cancelled()`
  from a real panic; only the latter prints and counts as a failure.
- **QA05-04 (RESOLVED — product bug in step 04's sink):** live `metrics_for`
  totals were stale at a quiet boundary because the coalesced accumulator was
  retried only on the next frame. Fixed with `has_pending()`/`retry_flush()`
  plus a guarded 100 ms `FlushTick` arm; guarded by a unit test, a
  live-visibility integration test, and e2e-03 **T4**. **Sprint 08/09 note
  (ADR-0018):** `metrics_for` is now live to within
  `metrics::FLUSH_RETRY_INTERVAL` (100 ms) of the stream and exact at
  `finish()` — a WS metric stream can poll it directly at any cadence ≥100 ms;
  nothing has to wait for a session to end.
- **QA05-05 (RESOLVED — test-side):** three scenarios settled on evidence that
  had not landed yet (pop→write window, a partial interrupt binding, the final
  `agent_start` racing a bare `Idle` assertion); all three settle predicates
  strengthened, no assertion weakened. **For QA (step 05/06):** these IDs were
  assigned by step 04 while resolving the QA05-02 residue — renumber or re-rule
  as you see fit. Note the pop→write window is *inherent* to at-most-once
  delivery, so **any** future test that waits on `queue_len() == 0` before
  reading the wire log or the charter will race the same way: prefer delivery
  evidence (`session-delivered` traces) or an explicit ack.
- Step 03 (RESOLVED — see Done/Decisions above): queue persistence is
  `courier/queues/<uid>.jsonl` behind `HamletTree::session_queue`;
  `Control` funnel extended with `Deliver`/`Interrupt`; brief goes through
  the queue. **Sprint 04 consumption points:** `mailroute` watches
  `courier/mail/<recipient>/` spools → `Supervisor::enqueue(uid,
  QueuedKind::Mail, rendered_text)`; `QueueError::Overflow` → bounce back
  to sender; `session-delivery-rejected`/`-failed` traces mark lost
  deliveries (at-most-once — a redelivery/bounce policy for those is
  Sprint 04's call); `Supervisor::queues().pending(uid)` renders queue
  state for CLI/bounces. Mail header rendering (From/To/Subject →
  envelope `text`) is Sprint 04's ADR.
- Step 04 (RESOLVED — ADR-0016/0017, see Done above): the latency-spike
  threshold is `sessions.latency_spike_ms` (config v4, default 30 000 ms);
  `parse_agent_event` was **not** changed — `message_end`,
  `tool_execution_*` and other unmodelled records are consumed through
  `AgentEvent::Other { record_type, raw }`, keeping `Other`/`Malformed`
  non-fatal. The interrupt join key is live: `<corr>:{q,s,a,p}` responses
  now produce `session-interrupt-acked` / `session-delivery-acked` traces
  plus `interrupt-ack` / `delivery-ack` latency spans.
  **Sprint 08/09 consumption points:**
  `Supervisor::metrics_for_{session,uid,commission}(…, window)` →
  `MetricsSnapshot` (already `Serialize`, sorted, clock-free — render it
  straight to a CLI table or a WS frame); `StreamStats::render()` is the
  `k=v` grammar of `session-stream-summary` if a UI wants to parse it.
  **Sprint 05 (adversarial) targets:** the per-life caps (8 malformed
  traces / 64 tool markers / 32 unknown types / 256 tool spans / 64
  pending acks / 64 coalesced records) and the lossless-additive rule —
  flood the stream and assert `dropped=` only ever counts records, never
  tokens or cost. **Open issue carried forward:** an oversized stdout frame
  is still allocated by `next_line()` before any size guard can apply (the
  reader is the step-02 `BufReader::lines()`); a capped incremental reader
  would close it, but that means touching the hot path the 24 existing
  supervision tests depend on — left for step 05 to judge.
- Sprint 05: Mayor sessions reuse this protocol with uid 1000
  (`SessionId` admits MAYOR_UID); Mayor wake packets are `tasking`
  envelopes; decree-driven interrupts use `source: mayor`.
- Sprint 07: hibernation consumes `stop_session`/`kill_session` (clean
  APIs, tested) and the Hibernating status (transition table already
  admits Running/Busy/Idle → Hibernating → Stopped/Crashed); `abandon()`
  is the kill-9 simulation for resumability tests.
- pi version drift: pi `--help` is authoritative per-version; the
  binding only uses commands documented in 0.99.1. A pi upgrade that
  renames events degrades to `Other` (non-fatal) — re-check docs on
  upgrades.

## Verify

```sh
cargo test -p hamlet-daemon --test adversarial_agent  # 12 QA scenarios incl. the QA05-02 regression (~1.0s)
./scripts/e2e-03-adversarial.sh                      # QA harness: T1 suite, T2 gates QA05-02 (10 samples), T3 secrets, T4 metrics-liveness wiring
cargo test -p hamlet-daemon --test metrics     # 11 telemetry scenarios (~1.0s)
cargo test -p hamlet-daemon --test delivery    # 10 queue + interrupt scenarios (~1.5s)
cargo test -p hamlet-daemon --test supervise   # 14 FakeAgent scenarios (~1.1s)
cargo test -p hamlet-daemon --lib              # 111 incl. metrics + snapshot goldens

# Flake discipline — this suite is race-heavy by design, so sample it; do not
# trust a single green run (QA05-02/04/05 were all found this way):
for i in $(seq 1 60); do cargo test -q -p hamlet-daemon --test adversarial_agent >/dev/null 2>&1 || echo "run $i FAILED"; done
cargo test -p hamlet-core session              # 20 protocol tests
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon --test supervise_guarded -- --ignored --nocapture
./scripts/ci-local.sh                          # CI battery: PASS
```
