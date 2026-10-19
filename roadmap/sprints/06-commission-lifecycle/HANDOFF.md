# HANDOFF — Sprint 05 The Mayor (Executive Control Loop)

Objective: `OBJECTIVE.md`. Prompts: `01-architect` … `06-commit-agent`.

## Step 01 — architect: decree vocabulary + file encodings + context packet

Status: **done**. Commit: `feat(core): decree vocabulary + context packet
schema (ADR-0011)` → actual ADRs **ADR-0026** (decree protocol) and
**ADR-0027** (context packet). The prompt cites "ADR-0011", but 0011 is the
host-permission model (claimed Sprint 02); per the `docs/adr/README.md`
renumbering rule this work is ADR-0026/0027 and the renumber table was
updated. **Later sprint-05 prompts reading "ADR-0011" must read
0026/0027.**

### Done

- **`hamlet-core::decree`** (SPEC §7; ADR-0026 cl. 1–8) — the normative
  vocabulary as one internally tagged `decree_type` enum: `approve_promotion`,
  `reject_promotion`, `hibernate_agent`, `provision_agent`, `reassign_task`,
  `steer_session`, `escalate_to_human`. Strict key allowlist per variant
  (`DecreeKind::allowed_fields`) replaces serde's unavailable
  variant-level `deny_unknown_fields` — unknown keys/tags are `Schema`
  rejections. Field-level typing at the trust boundary: `SafeName` refs
  (register/blueprint/artifact/indenture entry), `u32` slot uid,
  `TreeRelPath` for `freight_path` + `evidence_refs` (component-wise
  allowlist, ≤8 components, `.`/`..` rejected, `join_under` re-checks
  containment), single-line `summary`/`reason` (≤280, no control chars),
  LF-only `directive` (≤8 KiB, CR rejected like ADR-0019 cl. 1), 1–8
  evidence refs, `freight_path` must be under `courier/freight/` and name a
  file. `Decree::parse`/`render` byte-exact round-trip (sorted-key
  `serde_json::Value` at every level, pretty, trailing newline, ≤16 KiB).
- **Decree identity & replay algebra** — `DecreeId` = `d-{seq:012}-{digest16}`;
  `seq` = the charter's 1-based monotonic counter, `digest` = 16 hex chars of
  FNV-1a/64 over the canonical body **with `decree_id` removed** (dependency-
  free: SPEC §3 locks the crate stack; adversarial resistance is an
  explicit future ADR adding `sha2`). The file name is therefore a valid
  `SafeName` *and* lexicographically monotonic in `seq`. `Decree::stamp(seq)`
  is the exact transformation a stateless Mayor applies.
- **`hamlet-core::packet`** (ADR-0027) — typed `ContextPacket` = `MAYOR.md`
  verbatim + `StateGrid` (charter **subset**: `charter_version`,
  `next_decree_seq`, open decrees, commissions, parcels, sessions, slots —
  canonical sorted pretty JSON) + one `EventArtifact` (`MailReceived`,
  `WebhookEvent`, `FreightViolation`, `DecreeAckTimeout`, `CommissionCreated`,
  `GitDiffReady`; each with a ≤64 KiB verbatim body) + the `WakeReason`
  **derived** from the artifact so the two cannot disagree. Fixed section
  order, no history field, ends with a decree-protocol reminder block so the
  stateless executive can emit a legal decree without memory. Caps are
  refusals (`EmptyMayorMd`, `MayorMdTooLarge`, `EmptyArtifact`,
  `ArtifactTooLarge`, `PacketTooLarge`), never truncations.
- **`hamlet-core::tree`** — `manor_decrees()`,
  `manor_decree_file(&DecreeId)`, `manor_decree_ack(&DecreeId)`,
  `manor_decrees_rejected()`; `decrees/` added to `ensure` (static, like the
  dispatch dirs — a fresh-dir inotify race must not swallow the first
  decree). Goldens updated (`static_dirs` 24 → 25).
- **`hamlet-core::watch`** — ADR-0019 cl. 5 decree row **narrowed** (amendment
  recorded in ADR-0026 cl. 6): `Reaction::DecreeWrite` fires only for
  `keep/census/manor/home/decrees/<decree_id>.json`. `.ack` writes, scratch
  files and the rest of the Mayor home are `Ignored` — the daemon's own
  acknowledgments must never re-enter as a new decree.
- **`hamlet-core::charter` v2 → v3** (additive) — new group
  `decree_receipts: seq → digest`; `decrees` untouched so v2 files still load.
  New API: `next_decree_seq()`, `admit_decree(&DecreeId) ->
  DecreeAdmission{Admitted, Replayed}` (strict monotonic admission, one
  atomic save records receipt + queued together; identical replay = no-op;
  reused seq with different digest = `Invariant` refusal), and
  `set_decree_status`. Load-time invariants: receipts ⟷ status keys mirror,
  seq ≥ 1, digest = 16 lowercase hex.
- **`hamlet-core::config` v6 → v7** (additive) — `mayor: { ack_timeout_ms }`
  (default 30 000, range 1 000..=600 000), the ack-timeout → Mayor-wake
  policy (ADR-0026 cl. 5). Core + CLI goldens updated.

### Decisions (ADR refs)

- **Decree directory = `keep/census/manor/home/decrees/`** (container
  `/home/1000/decrees/`), *because* SPEC §4 mounts exactly one tree rw into
  the Mayor container — its own home. Everything else the Mayor "decides"
  (INDENTURE.md, vault copies, freight) is daemon-performed from a decree.
- **Ordering = charter counter, never mtime/clock** (CONVENTIONS §3 r4):
  `next_decree_seq()` renders into the packet, so the stateless Mayor always
  knows the one legal sequence. Out-of-order ids are refused, not reordered.
- **Replay protection is content-bound**: same `seq` + same digest = idempotent
  no-op (re-acked `done`); same `seq` + different digest = `replay_mismatch`
  ack. Without the digest a conflicting decree would be silently dropped.
- **Unparseable decree files have no identity, so they get no ack** — the
  daemon renames them into `.rejected/` and traces them (typed
  `RejectReason::Malformed` exists for files whose id *is* extractable).
- **FNV-1a/64, not sha256**: no new crate without an ADR, and the trust model
  here is ADR-0019's (the Mayor authors these files deliberately). Pinned
  vectors in `content_digest_golden`.
- **Decompose-commission decree deferred to Sprint 06** (OBJECTIVE scope
  boundary). The tag shape makes adding it an eighth variant a purely additive
  change — no amendment of this vocabulary needed, only a new ADR clause.
- **ADR cap deviation (disclosed):** `docs/adr/README.md` caps ADR bodies at
  40 lines. ADR-0026 lands at 63 and ADR-0027 at 45 body lines; the protocol
  is too dense to compress without dropping normative detail step 02/03 must
  build against. Precedent: ADR-0021 shipped at 64. Splitting further was
  rejected (coupled decisions); the split that *was* worth making (decree
  protocol vs packet schema) produced 0026/0027.

### Open issues / carry-forward

- **Step 02 (mayor session lifecycle + packets)** consumes ADR-0027: read
  `MAYOR.md` + charter, build `ContextPacket` per wake, feed it as a single
  turn to the pi session (no `--continue`, no accumulated thread), then
  sleep. Sources to wire from Sprint 04: mail-to-mayor, `WebhookWake`,
  `freight::MayorWake`, and (step 03) ack timeouts.
- **Step 03 (decree executor + wake triggers)** consumes ADR-0026: watch
  `Reaction::DecreeWrite`, scan `manor_decrees()` in sorted order, parse →
  `charter.admit_decree` → execute → write `<decree_id>.ack` atomically →
  `set_decree_status`. An unparseable file → rename into
  `manor_decrees_rejected()` + sled trace. The `decree_ack_timeout` wake uses
  `config.mayor.ack_timeout_ms`; escalate the once-per-process dedupe that
  ADR-0021 cl. 5 explicitly handed to Sprint 05 if stricter once-semantics
  are wanted.
- `Reaction::DecreeWrite` still carries only `path` (unchanged payload); the
  executor re-parses the id from the file name/body — no watch-table churn.
- **Sprint 06** adds `decompose_commission` + `INDENTURE.md` writing and may
  narrow the promotion-decree commission binding (today the prompt's field
  sets stand verbatim; the commission is resolved by the daemon).
- **Sprint 07** (hibernation) owns the `hibernate_agent` copy semantics;
  this step only fixed the vocabulary and the `uid` target shape.

### Verify

```sh
cargo test -p hamlet-core --lib decree    # 14: id/file-name goldens, digest input, render/parse
                                          # round-trip, parse-rejection + field-validation matrices,
                                          # ack goldens, replay algebra, traversal proof
cargo test -p hamlet-core --lib packet    # 6: state-grid golden, full packet golden, event header table,
                                          # determinism/history-free, cap refusals
cargo test -p hamlet-core --lib charter   # 18: v1→v3 / v2→v3 additive migration, admission + replay,
                                          # receipt invariants, kill/restart idempotence
cargo test -p hamlet-core --lib watch     # narrowed decree row + ack/scratch invisibility
cargo test -p hamlet-core --lib tree      # decree dir accessors, static_dirs 25
cargo test -p hamlet-core --lib config    # v6→v7 additive, ack_timeout range
./scripts/ci-local.sh                     # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…04
```

Workspace battery green (151 core + 6 packet + 18 charter unit tests and all
daemon/CLI suites). `ci-local.sh` reports one **environmental** red:
`[5/5] guarded podman tests` — the pre-existing QA04-07/08/09 host blockers
(dedicated service user, disk-backed `TMPDIR` + RAM headroom, podman
usability), unrelated to this change and skipped/RED before it.

---

## Step 02 — daemon-engineer: Mayor session lifecycle + context reconstitution

Status: **done**. Commit: `feat(daemon): mayor lifecycle + single-turn
context reconstitution (SPEC §7)`. Decisions recorded in **ADR-0028**
(this step's prompt says "ADR-0011", which is the host-permission model;
the packet protocol is ADR-0027 and the decree protocol ADR-0026, per the
step-01 renumber note above).

### Done

- **`defaults/MAYOR.md`** — the committed v1 trunk prompt (role; the
  "you act only by writing files" contract; the full decree vocabulary with
  the `seq`/`digest16` rules and an explicit instruction to abstain rather
  than guess a digest it cannot verify; validation duty; dual-state
  reconciliation between charter-as-decision-record and tree-as-world).
  Embedded with `include_str!` and seeded at provisioning.
- **`hamlet_daemon::mayor`** (lifecycle half; step 03 adds decrees):
  - `ManorProvisioner::provision` — `reserve_mayor` (UID 1000), manor +
    `home/` + `home/decrees/`, `MAYOR.md` seeded once (`create_new`,
    adopted when present, symlink/dir refused — ADR-0007), the mail spool,
    ADR-0011 braces, `RunSpec::for_mayor` + derived image through the
    `Runtime` seam, charter saved **last**. Idempotent: re-provisioning
    adopts the operator's `MAYOR.md` and never re-allocates the slot.
  - `HostPermissions::apply_manor_home` — 0700 + `unshare chown 1000:1000`,
    refusing every uid but 1000 (a manor home is not a parcel home).
  - `WakeEvent` constructors that **load the artifact from the tree**:
    mail spool (`Mail::parse` + canonical render), webhook inbox envelope
    (`InboxResponse`, status 202 marker), freight escalation, ack timeout,
    `COMMISSION.md`, git diff. Bodies capped at ADR-0027's artifact cap at
    construction.
  - `WakeQueue` — bounded (`mayor.wake_queue_depth`), dedupes identical
    artifacts, FIFO, overflow is a *counted refusal*
    (`WakeOutcome::Overflow`), never a silent drop.
  - `PacketBuilder` — `MAYOR.md` (lstat discipline) → `StateGrid::from_charter`
    → `ContextPacket::render`; every cap is a typed refusal
    (`MayorError::MissingMayorMd` / `Packet(..)`).
  - `MayorEngine` — `wake(event)` serializes on an async mutex (one
    evaluation at a time), takes a merged batch, renders the packet, runs
    **one** turn, and returns. `sleep_state()`, `recover()`,
    `queue_stats()`, plus `FreightWakeBridge` / `WebhookWakeBridge` for the
    Sprint-04 hooks.
- **Charter life per wake**: `sessions[1000]` is written
  `Starting → Running → Busy → Idle → Stopped` with **one incarnation per
  wake** (`SessionId::first` → `next_incarnation`), atomically. `recover()`
  heals an orphaned alive record to `Crashed` and is idempotent.

### Decisions (ADR-0028)

- **Fresh pi invocation per wake — NOT a parked session.** ADR-0015's
  finding is decisive: `--continue` resumes pi's own history, and one pi
  process answers many prompts in one context, so a parked process
  accumulates exactly the thread SPEC §7 forbids; pruning it would mean the
  daemon mutating the Mayor's private context. Therefore one wake = one
  process = one prompt = one turn, stdin EOF, reap. Between events the
  engine holds **no process** — zero-token sleep is structural. `--continue`
  is never passed (argv comes from `pi_session_cmd`); an overrunning turn is
  killed, never resumed.
- **Merged wakes are additive to ADR-0027**, not a change of it: a packet
  still carries ONE artifact; same-reason events are folded into it via the
  new `EventKind::MergedBatch` / `EventArtifact::merged` (per-member typed
  headers, `--- artifact <n> ---` fenced bodies). Five mails = one wake with
  five artifacts. Mixed reasons are refused (`MixedWakeReasons`) and wait
  for their own wake; a body that would exceed `MAX_ARTIFACT_BYTES` **splits
  across further wakes**.
- **Truncation policy** (as the prompt asked): nothing is ever truncated.
  Order of refusals is MAYOR.md cap → artifact cap → merged-batch cap
  (split, not clip) → packet cap. All four are typed errors; the prompt is
  never shortened.
- **Config v7 → v8 (additive)**: `mayor.wake_queue_depth` (1..=256, default
  16) and `mayor.turn_timeout_ms` (5 000..=1 800 000, default 120 000) join
  `mayor.ack_timeout_ms`. Core + CLI config goldens updated; a v7 document
  parses with defaults.
- **`MountSet::for_mayor()` is used verbatim** — manor home rw, mail,
  freight, pkgcache, context ro, and **no git bind**. The prompt's "ro
  git/context per active commission" does not apply: SPEC §4's Mayor row has
  no `/var/git` mount because the Mayor is multi-commission and receives
  diffs inside packets (`GitDiffReady` artifacts). Pinned by a golden.

### Verify

```sh
cargo test -p hamlet-core --lib packet    # 8: + merged-batch golden, merge refusals
cargo test -p hamlet-core --lib config    # 24: v7→v8 additive, queue/turn ranges, render golden
cargo test -p hamlet-daemon --lib hostperm # manor home 0700 + unshare chown 1000:1000
cargo test -p hamlet-daemon --lib mayor    # queue merge/dedupe/bound, batch split, no mixed reasons
cargo test -p hamlet-daemon --test mayor   # 9 FakeAgent-mayor integration tests:
                                           #   provisioning (shape, MAYOR.md golden, argv mounts, adopt),
                                           #   event → ONE turn with the byte-exact packet golden → idle-silent sleep,
                                           #   5 mails → 1 wake / 5 artifacts, bounded overflow,
                                           #   24-event storm → max_live == 1 (process-level serialization),
                                           #   restart → recover() heals, next wake = new incarnation, no thread,
                                           #   packet refusals (missing/empty/symlink MAYOR.md),
                                           #   freight + webhook hook bridges deliver tree-loaded artifacts
./scripts/ci-local.sh                     # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…04
```

Workspace battery green (154 core + 134 daemon unit + all integration suites).
`ci-local.sh` reports the same single **environmental** red as step 01:
`[5/5] guarded podman tests` (pre-existing QA04-07/08/09 host blockers — no
dedicated service user, disk-backed TMPDIR, podman unusable here), skipped
and RED before this change.

### Open issues / carry-forward

- **Step 03 (decree executor + wake triggers)** plugs into seams built here:
  `MayorEngine::wake(WakeEvent::from_decree_ack_timeout(..))`,
  `Reaction::DecreeWrite` → scan `manor_decrees()` → execute → ack, and the
  `freight::MayorWake` / `web::WebhookWake` bridges (replace the `Stderr*`
  sinks registered by Sprint 04). Nothing in the executor needs to touch
  session I/O.
- `WakeEvent::from_freight_escalation` carries `kinds: []` because
  `FreightEscalation` (Sprint 04) carries sandbox names, not typed
  `ViolationKind`s; step 03 should widen the audit payload if the Mayor is
  meant to reason over kinds.
- The packet's state grid is a snapshot taken **before** the turn's own
  session record exists, so the Mayor never sees itself mid-turn; a later
  wake does see the row. Pinned by the golden test.
- Two concurrently provisioned manor containers are an operator concern:
  `run_detached` fails on the taken name and the error surfaces (rollback
  never removes containers it did not start).

---

## Step 03 — daemon-engineer: decree executor + mayor wake triggers

Status: **done**. Commit: `feat(daemon): decree executor + mayor wake
triggers (SPEC §7, ADR-0011)` → the prompt's "ADR-0011" is the host-permission
model; this step's decisions are recorded in **ADR-0029** (decree execution
and wake triggers), on top of ADR-0026 (protocol) and ADR-0028 (lifecycle).

### Done

- **`hamlet_daemon::decrees` — the executor plane.** `DecreeExecutor::scan()`
  runs one directory pass in charter-sequence order: read (size-capped) →
  parse → pre-flight (every structural dependency resolved *before* any side
  effect) → `charter.admit_decree` (replay algebra, ADR-0026 cl. 3) → execute
  → `set_decree_status(Delivered)` + atomic `<decree_id>.ack`. Unparseable
  files are quarantined into `.rejected/` (never acked — they have no usable
  identity); out-of-order / replay-mismatch / invalid-target decrees get typed
  `rejected` acks and never consume their sequence. All seven decree kinds
  execute: provision (census `Provisioner` + `Supervisor::start_session`,
  brief = register + seeded PROPRIETOR), hibernate (stasis seam + charter
  lease flip), reassign_task / steer_session (session-router seam over the
  delivery queue + interrupt path), escalate_to_human (escalation notice +
  optional webhook), approve/reject_promotion (promotion seam). `spawn_decree_plane`
  starts the watcher, runs the **recovery pass**, and only then attaches the
  `Reaction::DecreeWrite` handler — a restart that left a half-executed decree
  finishes it before any new one can interleave.
- **Crash-mid-decree resume** is structural: admission writes receipt +
  `queued` in ONE atomic charter save (CONVENTIONS §3 r2); a daemon that dies
  between admission and execution leaves `queued`, and the next pass resumes
  with the idempotent executor. No special recovery path.
- **Wake triggers** (all config-driven, ADR-0029 cl. 1–2):
  - mail addressed to UID 1000 — `mailroute::MayorMail` hook seam hands the
    untouched spool path to the Mayor before the ack-rename;
  - inbound webhooks — `WebhookWakeBridge` fires only for topics present in
    `webhooks.routes`; an unconfigured topic never wakes;
  - freight audit violations — `FreightWakeBridge` (step 02) with the audit's
    own payload as artifact;
  - decree ack timeouts — `AckTimeouts` polls the decree dir and fires **once
    per process per decree** past `mayor.ack_timeout_ms`; an ack or a vanished
    file forgets it; `spawn_ack_timeout_watch` ticks at
    `min(timeout, 1 s)`;
  - session crashes — `supervise::CrashWake` seam (the supervisor *notifies*,
    never decides) + `CrashWakeBridge` filtered by `mayor.crash_wake`
    (`never` / `every` / `budget_exhausted`, default: only a crash whose
    restart budget is exhausted wakes). The wake artifact carries uid,
    session, restarts, permanent, and a scrubbed reason.
- **Escalation notices** (ADR-0029 cl. 4): an executed `escalate_to_human`
  writes one canonical JSON notice per decree at
  `manor/escalations/<decree_id>.json` (versioned, ≤8 KiB, evidence refs
  resolved in pre-flight) — the Mayor may write only `decrees/`, so the daemon
  files it where a human will look. `mayor.escalation_webhook_url` mirrors it
  to a human channel as a dispatch outbox request (https-only, validated
  before any write; replayed exactly once because the notice is the
  checkpoint).
- **`hamlet-core` addenda**: `EscalationNotice` + version/caps in
  `decree.rs`; `EventKind::SessionCrashed` + `WakeReason::SessionCrashed` in
  `packet.rs` (additive to ADR-0027); `manor_escalations`/
  `manor_escalation_file` in `tree.rs`; **config v8 → v9** (`mayor.crash_wake`,
  `mayor.escalation_webhook_url`).
- **FakeAgent `emit_files`** (test seam): the fake mayor can emit decree
  files mid-turn (atomic temp + rename, exactly as the real Mayor writes), so
  the full loop is testable without podman.

### Decisions (ADR-0029)

- **A *pass*, not a file event, is the reaction body.** The Mayor writes with
  temp+rename, so any watcher reaction can fire on a partial or duplicated
  view; a directory pass in charter order is idempotent no matter how many
  times it runs, and the execution lock guarantees no two passes overlap.
- **Stasis and promotion are seams with honest stubs** (ADR-0029 cl. 3):
  Sprint 06 ships the validation module, Sprint 07 the hibernation copies.
  The stubs acknowledge `deferred` — status stays `queued`, so the pass that
  ships the real seam executes the decree on a rescan. A stub never acks
  `done`.
- **The crash policy lives in config, evaluated by the Mayor plane** (cl. 2):
  supervision must not know a Mayor exists; the Mayor must not know how
  restart policies work. The `CrashWake` seam is notify-only.
- **Once-per-process ack-timeout wakes**: a Mayor that keeps re-emitting the
  same unanswered decree must not spin the wake queue; the memory resets on
  restart (conservative: the next process re-arms and may wake once more).

### Verify

```sh
cargo test -p hamlet-daemon --test decrees   # 10 integration tests:
                                             #   full fake loop (mail → wake → packet → FakeAgent-mayor
                                             #     emits decree mid-turn → real watcher reaction executes →
                                             #     escalation notice + done ack + charter Delivered → sleep),
                                             #   replay idempotence (re-ack byte-identical, no 2nd webhook POST),
                                             #   crash-mid-decree resume (charter left at queued completes),
                                             #   out-of-order + replay-mismatch refusals (no sequence consumed),
                                             #   invalid-target ack without side effects, quarantine of malformed
                                             #     files, reassign via router double, hibernate via stasis double,
                                             #     promotion deferred keeps status queued, unsupported-version
                                             #     quarantine with the mapped reason
cargo test -p hamlet-daemon --lib decrees    # 4: AckTimeouts truth table (once per process,
                                             #   forget on ack/vanish), reject_reason_of stable-token map,
                                             #   execution report counts
cargo test -p hamlet-daemon --test mayor     # + crash-wake policy test (restartable silent under
                                             #   budget_exhausted; permanent = one wake with the notice)
./scripts/ci-local.sh                        # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…04
```

Workspace battery green. `ci-local.sh` reports the same single
**environmental** red as steps 01–02: `[5/5] guarded podman tests`
(pre-existing QA04-07/08/09 host blockers).

### Open issues / carry-forward

- **Sprint 06**: replace `DeferredPromotion` with the validation module
  (vault copy + DEED entry), add `decompose_commission` as the eighth decree
  variant, and append INDENTURE entries to the provisioned session brief.
- **Sprint 07**: real `Stasis` implementation (hibernation copies); the seam
  and the charter lease flip are already in place.
- The freight wake still carries sandbox names, not typed violation kinds
  (`kinds: []`, step-02 carry-forward): widen the audit payload if the Mayor
  is meant to reason over kinds.

## Step 04 — qa-engineer: adversarial mayor suite

Status: **done, gate FAIL on one HIGH finding**. Commit:
`test(daemon): adversarial mayor suite + findings (SPEC §7, ADR-0011/0026/0029)`.
No product code was changed by this step — the working tree's only diff is
test/script/doc.

### Committed artifacts

- **`crates/hamlet-daemon/tests/adversarial_mayor.rs` — 16 attack tests,
  FakeAgent mayor, no podman.** Covers all seven prompt areas: forgery /
  injection, replay (incl. across restarts), zero-token sleep, packet
  hygiene, wake storms (500 events), executor crash resilience, escalation.
- **`crates/hamlet-daemon/tests/manor_guarded.rs` — guarded real-podman
  manor smoke** (CONVENTIONS §5): UID 1000 can author a decree and the
  daemon reads it back; UID 1001 is refused by the kernel every way round
  (absent mount, `..` traversal from a present bind root, gossip/freight
  traversal, spool forgery, infra sockets); plus the QA05-02 probe.
- **`scripts/e2e-05-mayor-adversarial.sh`** — T1 the suite, T2 seven
  source-level ratchets, T3 the guarded smoke. Runs in `ci.sh` via the
  `scripts/e2e-*.sh` glob.

### What the suite proved GREEN

- **Malformed / partial decrees**: a torn write, a JSON non-object, raw
  garbage with NUL/BOM, 400-deep nesting, and a 17 KiB body are all
  quarantined; **zero** executions, **zero** acks, **zero** sequences
  consumed, no escalation notice. "No partial execution" holds literally.
- **Digest binding (ADR-0026 cl. 3)**: editing a *delivered* decree in
  place, and re-labelling a different body onto the next sequence with the
  previous sequence's digest, are both refused. A forged `decree_id` cannot
  buy a fresh sequence.
- **Replay**: one execution across **three** restarts; byte-identical
  re-ack, unwritten notice, exactly one webhook POST. A Mayor-forged ack
  file (`done` for an unexecuted decree, `rejected` for a delivered one) is
  never state — the daemon is the governor.
- **Zero-token sleep**: over a monitored 1.5 s idle window the process table
  shows **no** Mayor child, `charter.json` is byte-identical, the session log
  gains not one byte, and no turn fires. An unconfigured webhook topic and
  `crash_wake = never` never wake.
- **Packet hygiene**: no `.env` value and no `.env` *key name*, no PEM
  block, no `.env` mention anywhere in the packet; the state grid's key set
  is pinned to the seven ADR-0027 keys.
- **Single-turn**: 10 sequential wakes = 10 invocations = 10 whole packets,
  no packet carries another turn's artifact, and `--continue` never appears
  in any recorded argv (ADR-0028 cl. 1).
- **Storm**: 500 events, `max_live == 1`, every event evaluated exactly once
  or explicitly refused (none vanish), depth never above cap, every merged
  batch FIFO by arrival.
- **Crash resilience**: `kill -9` mid-`ProvisionAgent` (slot durable, parcel
  half-built, no session) → restart resumes → **one** slot, **one** parcel
  entry, **one** lease, one successful completion; the interrupted decree
  was acknowledged rather than dropped.
- **Escalation**: notice is tree-observable, versioned, evidence refs
  re-resolved by the daemon, canonical bytes stable across replay, and the
  Mayor has no mount over `manor/escalations/` (ADR-0029 cl. 4).

### Findings

#### QA05-04 — HIGH — the ack `atomic_write` follows a planted symlink

`crates/hamlet-daemon/src/decrees.rs:1531-1550` (`atomic_write`) writes to
a **fixed, fully predictable** temp name `.{name}.tmp` (line 1541) with
`OpenOptions::new().write(true).create(true).truncate(true).open(&temp)`,
which **follows symlinks**.

Repro (committed, green, in
`tests/adversarial_mayor.rs::qa05_04_ack_atomic_write_follows_a_planted_symlink_temp_file`):

```sh
cargo test -p hamlet-daemon --test adversarial_mayor qa05_04
```

The Mayor owns exactly one writable place in the village —
`keep/census/manor/home/decrees/` — and its prompt is an untrusted artifact
stream (mail to UID 1000). It plants
`{decree_dir}/.{decree_id}.ack.tmp` as a symlink to `../../../../charter.json`,
writes any well-formed decree, and the **daemon** truncates and overwrites
`charter.json` with the ack bytes. Verified end state:
`Charter::load(charter.json)` fails — the single serialized state map
(SPEC §1 file-as-truth) is destroyed, and every subsystem fails closed.

Expected: an ack temp file the daemon creates itself, opened `O_NOFOLLOW` /
`O_EXCL`, or written into a directory the Mayor cannot pre-populate.
Actual: any file the daemon user can write is reachable through one symlink.

SPEC §1 (file-as-truth), §2 (typed paths), CONVENTIONS §2 (sanitizer at the
boundary), ADR-0007 (the no-symlink rule the same module honours in
`require_regular_file`, `decrees.rs:1101`). Note the same helper is used for
the escalation notice (`:1303`, `:1338`) — those land in
`manor/escalations/`, which the Mayor cannot reach, so `write_ack`
(`:1446`) is the exposure today; the helper should be fixed once for all.

#### QA05-02 — HIGH (deployment) — the daemon cannot write the ack into the
0700 manor decree directory outside `podman unshare`

`HostPermissions::apply_manor_home` (`crates/hamlet-daemon/src/hostperm.rs:169-175`)
sets `manor/home/` to 0700 and chowns it via `unshare_chown(1000, 1000, …)`,
i.e. to the **sub**uid 1000 (ADR-0011 cl. 2/3). `decrees/` lives inside it.
ADR-0011 cl. 4 states the consequence outright: *"the daemon host user cannot
directly read subuid-owned 0700 homes/spools … go through `podman unshare`."*
`DecreeExecutor::write_ack` (`:1446`) and `quarantine` (`:1468`) use plain
`fs` calls with no unshare wrapper — the precedent Sprint 04's mail plane
and Sprint 07's hibernation copies set is simply not wired here.

Failure mode when the daemon runs under the spec'd dedicated service user:
the decree **executes**, `set_decree_status(Delivered)` persists, and the
ack write fails with `EACCES` — but `:1125`/`:1140` only *trace*
(`decree-ack-write-failed`). The Mayor never receives its ack, the ack-
timeout watcher sees an unacknowledged file, and the whole loop silently
stalls after one turn.

Repro: the guarded probe
`tests/manor_guarded.rs::podman_probe_daemon_can_write_the_ack_into_the_manor_decrees_dir`
(prints LATENT vs LIVE for whichever deployment shape the host has):

```sh
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon --test manor_guarded \
  -- --ignored --nocapture
```

On this dev host `id -u` is 1000, so the write succeeds and the bug is
**latent**; that is exactly why it survived steps 02–03.

Expected: the decree plane's writes into `manor/home/` go through the same
`podman unshare` seam the mail plane uses, or the failure is a hard,
surfaced error rather than a trace. Actual: silent.

SPEC §4 (manor mount row), §7 ("the daemon interprets"), ADR-0011 cl. 3–4,
CONVENTIONS §2 (singular governor). Adjacent carry-forward, **not** filed
against sprint 05: `mailroute.rs` writes into 0700 subuid-owned spools the
same way — Sprint 04/07 scope.

#### QA05-01 — MEDIUM — the executor verifies no decree provenance

`DecreeExecutor::read_decree` (`crates/hamlet-daemon/src/decrees.rs:962-990`)
validates exactly three things: `symlink_metadata` regular-file check, the
16 KiB size cap, and `Decree::parse`. There is **no** owner comparison
against `MAYOR_UID`, no refusal of a group/world-writable decree directory,
and no `O_NOFOLLOW` open. ADR-0029 names the Mayor as the only intended
author.

Repro: `tests/adversarial_mayor.rs::qa05_01_decree_provenance_is_unverified_and_a_foreign_writer_is_executed`
replaces `manor/home/decrees/` with a symlink to an attacker directory
outside the village; the planted decree executes and is acked `done`, and
the daemon files the escalation notice for a decree it cannot attribute.

Severity is **defense-in-depth**, not exploit: `apply_manor_home` (0700) and
`MountSet::for_agent` ("Manor home is Mayor-only and absent") both hold, and
the suite asserts them. But SPEC §2 makes the tree the contract and
CONVENTIONS §2 requires sanitization *at the boundary*; today the boundary
is a permission, not a check, so a future mount widening fails open and
silently.

Expected: `read_decree` refuses a decree whose owner is not the mayor slot
(and whose directory is not owned by it), with a typed `RejectReason`.
Actual: executed. Fix shape is one `MetadataExt::uid()` comparison plus an
`O_NOFOLLOW` open, behind the same `require_regular_file` helper.

#### QA05-03 — MEDIUM — ack-timeout wakes fire once per *process*, not once
per timeout

`AckTimeouts` (`crates/hamlet-daemon/src/decrees.rs:1723-1785`) keeps
`woken: BTreeSet<DecreeId>` (`:1725`) and inserts at `:1774`; nothing clears
it while the file lives. A decree that stays unacknowledged for a day
therefore escalates to the Mayor **once** for the lifetime of the daemon
process; only a restart re-arms it.

Repro: `tests/adversarial_mayor.rs::qa05_03_ack_timeout_wakes_once_per_process_not_once_per_timeout`
(pure, no sleeping daemon): 11 timeouts past the threshold → 1 wake.

This contradicts the prompt's requirement "ack timeouts re-wake the Mayor
exactly once **per timeout**". ADR-0029's *goal* (don't let a pathological
decree spin the queue) is met, but the policy has no operator escape other
than restarting the daemon, and no `mayor.` config knob controls the
re-notification interval. Expected: either a per-timeout re-wake with an
exponential/`mayor.ack_retry_ms` backoff, or the once-per-process semantics
stated explicitly as operator-visible config.

SPEC §7 (event-driven wakes), ADR-0029 cl. 1.

#### QA05-05 — LOW — quarantine is destructive, so a torn in-place write
loses the decree

`quarantine` (`decrees.rs:1468-1487`) is `fs::rename`. MAYOR.md tells the
Mayor to write temp-then-rename, so a conforming Mayor never trips this — but
an LLM that ignores the instruction (or any non-atomic writer) has its
half-written decree moved to `.rejected/` while its remaining bytes land in
the quarantine inode. The decree is then unexecutable forever and there is
**no ack**, so the Mayor's only signal is a timeout.

Repro: `tests/adversarial_mayor.rs::qa05_05_a_torn_in_place_write_is_destructively_quarantined`.

Expected: a file that becomes parseable later is re-scanned (retry with
backoff before quarantine), or a `.partial`/`.tmp` name is treated as
"not yet a decree" rather than "malformed". Actual: irreversible rename.

ADR-0026 cl. 5 (quarantine), SPEC §7.

#### QA05-06 — LOW — the wake queue's duplicate rule conflates two distinct
events with one artifact

`WakeQueue::push` (`crates/hamlet-daemon/src/mayor.rs:854-856`) treats
`artifact == event.artifact` as a duplicate arrival. `EventArtifact`
equality is over the rendered `(kind, body)` pair, so two *different*
real-world events that serialize identically — the same sandbox quarantined
twice for the same owner, two identical crash notices — collapse into one
wake. Impact is bounded to events pending simultaneously (the queue drains
at the start of each turn), but a slow turn is a wide window, and the Mayor
is never told the second violation happened.

Repro: `tests/adversarial_mayor.rs::qa05_06_identical_artifacts_from_distinct_events_are_merged_away`.

Expected: dedupe on a source identity (spool path / correlation id /
audit id), not on rendered content. ADR-0028 cl. 3, SPEC §7.

#### OBS-05-04 — LOW — the escalation / decree CLI hook does not exist yet

SPEC §12.1 lists `decree {list|show}`; `crates/hamlet-cli/src/main.rs:5`
mentions it only in the module doc and `:10` carries
`// TODO(sprint 08): clap argument parser + subcommands`. `hamlet decree
list` / `show <decree_id>` / escalations cannot be read without reading the
tree by hand. This is **Sprint 08 scope** per SPEC §12.1, so it is not a
sprint-05 defect; it is pinned in the e2e T2 ratchet as a recorded gap so
it cannot be mistaken for delivered work. Prompt item 7's "CLI hook" half is
therefore **not delivered**; the tree half is, and is tested.

### Gate

**`GATE 05/04: FAIL`** (superseded-in-part by step 03b: QA05-04 is fixed and
its repro test inverted into a regression; QA05-02 is half-fixed — loud
failure, write seam still owed. Re-run this step to re-gate.) — **superseded
by step 04b: re-gated PASS on the final tree** (full e2e-05 green in battery
`logs/ci-gate-qa-final.log`; see that step for the record) —
QA05-04 (HIGH, repro'd in a committed test,
fixable in ~10 lines inside `decrees.rs::atomic_write`) and QA05-02 (HIGH,
deployment-blocking under the spec'd service user, needs a `podman unshare`
write seam) are open product defects in code this sprint shipped. QA is
explicitly barred from fixing product code, so both are filed here rather
than patched. QA05-01/03/05/06 are MEDIUM/LOW and non-blocking but should
be scheduled. Sprint 05 cannot be tagged until QA05-04 and QA05-02 land.

### Verify

```sh
cargo test -p hamlet-daemon --test adversarial_mayor    # 16 attack tests
cargo test -p hamlet-daemon --test manor_guarded         # 2 guarded, #[ignore]d
./scripts/e2e-05-mayor-adversarial.sh                    # T1 suite + T2 ratchets + T3 guarded
./scripts/ci-local.sh                                    # fmt, clippy -D warnings, workspace,
                                                         # check-deps, e2e-01…05
```

T2 ratchets were **mutation-tested** (QA05-08): replacing
`let meta = fs::symlink_metadata(path)` with `fs::metadata` (decrees.rs:963),
un-dot-prefixing the ack temp name (:1541), renaming the
`decree-ack-write-failed` trace (:1125/:1140), and adding the word
`escalations` to `mount.rs` each turn the harness RED. The first of those
mutations was initially invisible because the grep matched
`require_regular_file`'s stat too — the ratchet is now anchored on `let meta =`.

### Open issues / carry-forward

- **Sprint 05 (blocking)**: QA05-04 and QA05-02 above — both addressed in
  **step 03b** (QA05-04 fully; QA05-02 half: hard error now, `podman unshare`
  write seam still owed). Sprint 05's gate verdict therefore stays FAIL until
  QA re-runs step 04's suite against the patched executor.
- **Sprint 06/07**: QA05-01 should be fixed before promotion and hibernation
  land — those two decrees are the first ones whose *execution* has
  consequences outside the tree (vault copies, `tavern/patrons/` homes), so
  an unattributable decree author becomes materially more dangerous.
- **Sprint 08**: OBS-05-04 (the `decree {list|show}` / escalation CLI hook).
- **Sprint 04/07 (not sprint 05)**: `mailroute.rs` writes into 0700
  subuid-owned mail spools with plain `fs` calls — the same QA05-02 shape.
- `e2e-05` was green here only because this host's `id -u` is 1000: any CI that
  runs the daemon as a dedicated non-1000 service user will exercise
  QA05-02 for real. The QA05-02 probe exists to make that visible.

---

## Step 03b — daemon-engineer: QA05-04 / QA05-02 product fixes

Status: **done**. Commit: `fix(daemon): ADR-0007 create-new temp writes and
surfaced ack failures (SPEC §7, ADR-0007/0011/0029)`. QA (step 04) filed two
HIGH findings against the step-03 executor; both are product defects in code
this sprint shipped, so the owning role patched them. QA05-01/03/05/06 remain
MEDIUM/LOW and are still carried.

### Done

- **QA05-04 — `atomic_write` followed a planted symlink (FIXED).**
  `crates/hamlet-daemon/src/decrees.rs::atomic_write` opened its temp file at
  a fully predictable name (`.{name}.tmp`) with `create(true).truncate(true)`,
  which follows symlinks. The Mayor owns `manor/home/decrees/` and its input
  stream is untrusted (mail to UID 1000), so one planted symlink made the
  **daemon** truncate-and-overwrite any file the daemon can write —
  `charter.json` included, i.e. total loss of the village's only serialized
  state map (SPEC §1). Fix: ADR-0007 **create-new discipline** —
  `OpenOptions::new().write(true).create_new(true)` (`O_CREAT|O_EXCL`, which
  fails on an existing entry *including a symlink*) — plus a bounded
  collision walk (`.tmp`, `.r1.tmp`, … `TEMP_ATTEMPTS = 256`) and a typed
  exhaustion error. A planted name is **skipped, never opened**, and there is
  no fallback to an unsafe write. The same helper serves the ack, the
  `.rejected/` rename staging and the escalation notice / webhook outbox
  writes, so the fix lands once for all.
  The finding's repro test is inverted into a standing regression:
  `tests/adversarial_mayor.rs::qa05_04_ack_write_refuses_a_planted_symlink_temp_file`
  asserts the charter stays loadable and carrying its own groups, the planted
  symlink is untouched, and the ack lands on the real path.
- **QA05-02 — a lost ack is now a HARD error, not a trace (partial fix).**
  `manor/home/` is 0700 and owned by subuid 1000 (ADR-0011 cl. 2/3), so under
  the spec'd dedicated service user the daemon's direct ack write is refused
  by the kernel — and the old code only *traced* it, stalling the Mayor loop
  silently. `execute()` now propagates the failure (`self.write_ack(&DecreeAck::done(&id))?`),
  `process()` returns it, and `scan()` **continues over the remaining files**
  and reports the first error at the end — so nothing is dropped and the
  failure is loud. A replay re-attempts the write (`Replayed + Delivered`
  re-acks), so recovery is automatic once the permission problem is gone.
- **`scripts/e2e-05-mayor-adversarial.sh` T2 ratchets re-anchored** on the
  new invariants (mutation-tested in the same spirit as QA05-08):
  `create_new(true).open(&temp)` present **and** no `.create(true)` (symlink
  capture), and the ack write is `?`-propagated with the
  `decree-ack-write-failed` trace **gone** (silent-trace regression).

### Decisions

- **create-new, not O_NOFOLLOW-on-open**: `O_EXCL` gives the stronger
  property for free — the temp file is provably one *this call* created, so
  there is nothing to re-stat and nothing to race. It matches
  `mayor.rs::seed_mayor_md` (the ADR-0007 precedent in this codebase).
- **A denial-of-service is acceptable, an escape is not**: if the Mayor plants
  all 256 names the ack write fails with a typed error and the pass reports
  it. The alternative — reusing or truncating a name we did not create — is
  the vulnerability itself.

### Open issues / carry-forward

- **Sprint 05 (still open, deployment shape):** the *proper* QA05-02 fix is a
  `podman unshare` write seam — ADR-0011 cl. 4 requires the daemon to write
  subuid-owned trees from inside its userns, and `Runtime::unshare_exec`
  currently passes `None` for stdin, so bytes cannot be piped through it. It
  needs a typed `unshare_write(bytes, path)` helper (never a shell — ADR-0010
  cl. 1) wired into `atomic_write`'s permission-denied branch, with the
  already-committed guarded probe
  (`tests/manor_guarded.rs::podman_probe_daemon_can_write_the_ack_into_the_manor_decrees_dir`)
  as the acceptance test. Until then the failure is loud, which is the
  finding's stated acceptable alternative. Same class as the `mailroute.rs`
  carry-forward QA noted is Sprint 04/07 scope.
- **Sprint 06/07**: QA05-01 (decree provenance: no owner check on the decree
  directory, no `O_NOFOLLOW`) — unchanged, still the top follow-up because
  promotion and hibernation are the first decrees whose execution escapes the
  tree.
- **QA05-03/05/06** (MEDIUM/LOW) unchanged.

### Verify

```sh
cargo test -p hamlet-daemon --test adversarial_mayor qa05_04   # regression, green
cargo test -p hamlet-daemon --test decrees                      # 10 executor tests
./scripts/e2e-05-mayor-adversarial.sh                           # T1 + re-anchored T2
./scripts/ci-local.sh                                           # fmt, clippy -D warnings, workspace,
                                                                # check-deps, e2e-01…05
```

---

## Step 05 — daemon-engineer: QA05-01/02/03/05/06 boundary fixes

Status: **done**. Commit: `fix(daemon): decree boundary hardening —
provenance, unshare writes, stable-file quarantine, wake identity (SPEC §7,
ADR-0030)`. Decisions recorded in **ADR-0030** (new), on top of ADR-0026
(protocol), ADR-0028 (wake queue) and ADR-0029 (execution/triggers).

The gate blocker QA05-04 landed in step 03b; this step closes the remaining
findings — both HIGHs (QA05-02 fully, its write half), all three MEDIUMs and
both LOWs of the step-04 suite, with the finding tests inverted into
regressions.

### Done

- **QA05-02 (HIGH, deployment) — the unshare write seam.**
  `crates/hamlet-daemon/src/podman.rs` gains five typed payload builders
  (`mktemp_staged_cmd`, `cp_staged_cmd`, `chmod_staged_cmd`, `chown_one_cmd`,
  `mv_staged_cmd`) and `Runtime::unshare_write_atomic(dest, bytes, owner)`
  (default impl *refuses*, so a runtime without the seam says so). The real
  runtime stages the bytes in a daemon-private `O_EXCL` file, then runs
  `podman unshare mktemp` → `cp` → `chmod 0600` → `chown 1000:1000` → `mv`:
  the staging name is `mktemp`'s **unpredictable** one (ADR-0007 create-new
  semantics the daemon cannot have in a directory the Mayor owns), every
  path slot is behind `--`, `mktemp`'s printed path is re-validated, and the
  final step is a same-directory rename. Never a shell (ADR-0010 cl. 1);
  ADR-0011 cl. 4 satisfied.
  `DecreeExecutor::with_runtime` installs it and `write_atomic` tries the
  direct write first, escalating **only** on `PermissionDenied`. Ack,
  quarantine rename, quarantine mkdir, the escalation notice and the dispatch
  outbox request all go through it. With no runtime installed the original
  error still surfaces — loud, never a trace (step 03b's guarantee).
- **QA05-01 (MEDIUM) — decree provenance.** `verify_decree_dir` runs before
  any file is read: `manor/home/decrees/` must be a plain directory (never a
  planted symlink), must canonicalize inside the manor home, and must not be
  group/world-writable. `read_decree` then requires each file to be owned by
  that directory's owner, to `lstat` as a regular file, and to open
  `O_NOFOLLOW` with the **opened handle's** metadata re-checked. Failure
  yields the new `RejectReason::ForeignProvenance` and
  `DecreeOutcome::Unattributable` — left exactly in place: not executed, not
  acked, not quarantined, no sequence consumed. A directory failure fails the
  whole pass closed (`ExecutorError::Provenance`). The comparison is against
  the *directory's* owner, not a hardcoded `MAYOR_UID`, because under
  rootless podman subuid 1000 is some other number on the host (ADR-0011
  cl. 2).
- **QA05-03 (MEDIUM) — ack timeouts are per timeout.** `AckTimeouts` replaces
  the never-cleared `woken` set with `(last_fired, notice_count)` and a new
  config knob **`mayor.ack_retry_ms`** (config v9 → v10, 1 000..=7 200 000,
  default 600 000). Threshold = "has the Mayor had a fair chance?", cadence =
  "is it worth asking again?" — both operator config, both pure daemon-side
  behavior. The wake artifact body carries the notice number. ADR-0021 cl. 5's
  anti-spin goal is preserved by the default cadence being 20× the threshold.
- **QA05-05 (LOW) — quarantine needs two observations.** A parse failure
  records `(len, mtime)` and reports `DecreeOutcome::ParseDeferred`; only a
  later pass seeing the identical shape quarantines. A torn in-place write is
  therefore left alone, finishes, and executes — proven end-to-end by the
  inverted test — while a permanently bad file is still quarantined, one pass
  later than before.
- **QA05-06 (LOW) — wake identity is per-plane source identity.** `WakeEvent`
  gains `WakeSource` (spool path / webhook correlation / `decree_id#notice` /
  `uid/session/restarts` / git triple / freight `owner#audit_id`), and
  `WakeQueue::push` dedupes on that instead of rendered artifact equality.
  `FreightEscalation` gains the `audit_id` it never had — the plane could not
  previously distinguish two audits of one violation at all. The identity is
  queue bookkeeping and never reaches a packet.

### Decisions (ADR-0030)

- **Every one of these was a decision made on the wrong fact** — content
  instead of origin, one shot instead of observed stability, process lifetime
  instead of operator cadence. The fixes are uniform: name the fact, observe
  it twice where it can change, and make the policy a number an operator can
  read.
- **Refuse loudly when there is nothing to fall back to.** With no runtime
  installed the direct-`PermissionDenied` still surfaces as a hard error;
  step 03b's "never a silent trace" is kept as the floor under the new seam.
- **An unattributable file is never cleaned up.** Quarantining a file the
  daemon cannot prove the Mayor wrote is a write into a directory it may not
  own, made at exactly the moment it should trust nothing.
- **`O_NOFOLLOW` as a named constant, not a `libc` dependency** — SPEC §3
  locks the crate stack, and an unknown open flag fails loudly on Linux rather
  than being ignored.
- **Two-step quarantine is a visible behaviour change:** a test that asserted
  "one scan quarantines" now scans twice. That gap is the difference between
  destroying a write in flight and giving it a chance to finish.

### Verify

```sh
cargo test -p hamlet-core --lib config     # 30: +v9→v10 additive, ack_retry range, render golden
cargo test -p hamlet-core --lib decree     # +foreign_provenance token, additive
cargo test -p hamlet-daemon --lib          # 141: AckTimeouts per-timeout table, podman argv
                                           #   goldens + injection matrix, queue dedupe by source
cargo test -p hamlet-daemon --test decrees        # 11: +QA05-02 seam regression (real EACCES)
cargo test -p hamlet-daemon --test adversarial_mayor  # 17: QA05-01 (a)+(b), QA05-03, QA05-05,
                                           #   QA05-06 inverted into regressions
./scripts/e2e-05-mayor-adversarial.sh      # T1 + 9 re-anchored/mutation-tested T2 ratchets
./scripts/ci-local.sh                      # fmt, clippy -D warnings, workspace, check-deps, e2e-01…05
```

T2 ratchets **mutation-tested** (QA05-08 discipline): renaming
`verify_decree_dir`, reverting dedupe to `q.artifact == event.artifact`,
changing the unshare call's owner argument, restoring the `woken: BTreeSet`
field, and deleting the `O_NOFOLLOW` open each turn the harness RED — the
last one only after re-anchoring the grep on `.custom_flags(libc_nofollow())`
(the first anchor matched the helper's own definition and was invisible).

### Open issues / carry-forward

- **Sprint 04/07 (not sprint 05)**: `mailroute.rs` writes into 0700
  subuid-owned mail spools with plain `fs` calls — the same QA05-02 shape.
  The seam now exists (`Runtime::unshare_write_atomic`) and can be reused.
- **Sprint 08**: OBS-05-04, the `decree {list|show}` / escalation CLI hook —
  still the recorded gap, still pinned by an e2e T2 ratchet.
- **Sprint 06/07**: nothing of QA05-01 is deferred — the promotion and
  hibernation decrees now inherit the provenance check rather than opening a
  hole.
- `ci-local.sh` still reports the single **environmental** red recorded since
  step 01: `[5/5] guarded podman tests` (QA04-07/08/09 host blockers: no
  dedicated service user, disk-backed `TMPDIR`, podman unusable here). The
  QA05-02 seam probe in `manor_guarded.rs` can only be *executed* on such a
  host; its unshare half is covered by argv goldens, injection matrices, the
  `FakeRuntime::unshare_writes` integration test and the 0500-directory
  regression elsewhere.

---

## Step 05b — ci-agent: sprint 05 gate (podman-capable job + disk-leak policy)

Precondition: step 05 recorded its fixes. Host/CI residual only — **no
product code was touched**; every edit is in `scripts/` (CI config).

### What was wrong with the gate itself

Three defects in the *harness*, all of which had been reported as "the guarded
battery is environmental" since sprint 04:

1. **The guarded battery could not run, only skip.** `ci.sh` step 5 executed
   `cargo test --workspace -- --ignored podman` as *whatever user happened to
   be at the keyboard*, with that user's `TMPDIR`. On this host `/tmp` is a
   7.5 GB **tmpfs**, so QA04-08's precondition was unmet every single time and
   all seven guarded suites skipped — while the battery reported PASS. The
   remedy (ADR-0032 cl. 4, `guarded-service-user.sh`) existed, but it lived in
   a different file from the caller that needed it and nothing pointed at it:
   `ci.sh` probed only `podman info`, and each e2e harness asked its own
   question in its own words ("`HAMLET_PODMAN_TESTS != 1`", "podman not on
   PATH", "no `/etc/subuid` entry") and each answered *skip*.
2. **`set -e` meant one red hid every other gate.** A red step 1/2/3/4 aborted
   the script before step 5 and before the e2e loop — the exact failure mode
   QA04-08's carry-forward was filed about, still unfixed one layer up. Proven
   live during this step: a `cargo fmt` red (a concurrent session's in-flight
   test file) produced a log containing *only* step 1.
3. **An aborted guarded run silently eats ~18 GB.** The village tree is built
   with `tempfile` under `$TMPDIR` and then handed to **subuids**
   (`podman unshare chown`, ADR-0011 cl. 2). A subuid-owned file cannot be
   unlinked by the owner outside the userns — and not by root either, because
   the parent directories are 0700 and subuid-owned. Nothing in CI ever said
   so; the disk simply filled up between runs.

### Changes (CI config only)

| File | Change |
|---|---|
| `scripts/guarded-entry.sh` **(new)** | The single podman-capable-job entry point. Decides by measurement: already the service user → run here; current user passes `guarded-service-user.sh --doctor` → run here; `sudo -n` available → hand the battery to `hamlet`; otherwise **exit 3 with the exact command**, never a vacuous green. |
| `scripts/ci-guarded-job.sh` **(new, sourced)** | Caller-side helpers so no harness re-implements the ladder: `ci_guarded_direct_viable` (is *this* user able to run a suite directly?), `ci_guarded_delegate` (run it as whoever can), `ci_guarded_disk_audit`, and the one-job-per-host lock. |
| `scripts/village-reclaim.sh` **(new)** | The disk-leak policy as an executable document: `--audit` (stranded trees + free space, printed **before** the battery), `--reclaim` (`podman unshare rm -rf`, guarded against a mis-set `TMPDIR`), `--policy`. |
| `scripts/ci.sh` | Step 5 rewritten around the entry point; **every step now records its own verdict** and the battery prints one PASS/FAIL table at the end and exits non-zero on any red. Output is streamed (`tee`), because the battery is ~25 min of silence otherwise. |
| `scripts/e2e-02/04/05` | Their guarded sections no longer skip: when the direct route is closed they **delegate** to the entry point, and a skip is recorded only when *no* route exists — with the remedy printed. |
| `scripts/e2e-01-topology-state.sh` | New T8: guarded-battery **accounting** (asserts nothing; prints the run command when the job is not reachable here). |
| `scripts/e2e-01-safename-compile-fail.sh` | Header states why it needs no service user (`rustc` + rlib, privilege-free) instead of leaving the gap implicit. |

### Findings

- **QA05-09 (CI, HIGH — was silently reading as a product defect).** Guarded
  container names are **global per podman user**. Two concurrent guarded jobs
  on one host delete each other's containers mid-assertion. Measured twice on
  this host, 60 s apart:

  ```
  # battery A (12:50) vs a concurrent `cargo test --test bootstrap_guarded` (12:51)
  node probe failed (exit 255): stderr="Error: can only create exec sessions on
    running containers: container state improper"
  ROLLBACK INCOMPLETE: [remove /var/hamlet/tmp/.tmpWXe7RN/.hamlet/keep/census/parcel1001:
    Permission denied (os error 13)]

  # battery A (12:05) vs a concurrent battery B (12:11), escape_guarded:
  "no container with ID 5cafd495... found in database: no such container"
  ```

  Both suites are **green when run alone** (see the table: `bootstrap_guarded`
  4/4, `escape_guarded` 2/2). CI-side fix in place: every CI-driven guarded
  invocation takes `flock` on `$HAMLET_GUARDED_LOCK` and a second job
  **declines with the live holder's pid named** instead of colliding (QA04-20
  generalised from "two tests in one binary" to "two jobs on one host").
  **Residual (not fixable in CI):** a human typing
  `cargo test -- --ignored podman` by hand bypasses the lock. One guarded job
  per host, per podman user, is now a stated rule.
- **QA05-10 (CI, MEDIUM — the disk-leak policy).** Stranded village trees are
  reclaimable *only* by their owner inside the userns. Measured this step: 9
  stranded trees under `/var/hamlet/tmp`; `--reclaim` removed every
  `dock`-owned one (including a tree whose rollback had aborted with
  `ROLLBACK INCOMPLETE`) and correctly refused the `hamlet`-owned ones with
  `sudo -H -u hamlet podman unshare rm -rf <tree>`. Policy, now explicit in
  `village-reclaim.sh` and in `ci.sh` step 5a: read free space **before** the
  run, reclaim after any abort, and never treat "the disk filled up" as
  weather.
- **`guard_acceptance.rs` cannot be run by `cargo test --workspace` (test-side,
  in flight).** The new guard-acceptance suite (from the QA session working
  this sprint in parallel) is driven by env that **only**
  `scripts/e2e-06-guard-mutations.sh` exports, so step 3 is red:

  ```
  thread 'a_chown_that_changes_nothing_is_not_subuid_support' panicked at
    crates/hamlet-daemon/tests/guard_acceptance.rs:44: QA_GUARD_SHIM_DIR set by e2e-06: NotPresent
  thread 'the_work_area_guard_separates_ram_from_disk' panicked at
    crates/hamlet-daemon/tests/guard_acceptance.rs:264: QA_GUARD_DISK_AREA: NotPresent
  test result: FAILED. 4 passed; 6 failed
  ```

  `e2e-06` itself is green. Per the repo's own doctrine the fix belongs to the
  suite: when `QA_GUARD_SHIM_DIR`/`QA_GUARD_DISK_AREA` are unset it must
  **record a SKIP**, not panic — otherwise the workspace gate is red for a
  harness that is not even supposed to run here. Not fixed here: that file is
  a test, and this role does not edit tests.

### Gate battery (final tree, this host, uncontended)

`TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 ./scripts/ci-local.sh`
— log: `logs/ci-gate.log`. "Suites" are the per-suite results inside step 5.

| # | Check | Result | Notes |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **PASS** | clean |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | clean |
| 3 | `cargo test --workspace` | **FAIL (test-side)** | `guard_acceptance.rs` only — 6 tests need e2e-06's env; minimal output above. Not product code. |
| 4 | `scripts/check-deps.sh` | **PASS** | allowlist intact |
| 5a | village-tree disk policy | **PASS** | stranded trees enumerated; floor met |
| 5 | guarded podman battery | **PASS** | **all seven suites ran for the first time**: podman 1, bootstrap 4, provision 1, escape 2, freight 1, supervise 1, manor 2 — 12 tests, no skips |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | type-enforced boundary |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | T1–T7 + new T8 accounting |
| 8 | `scripts/e2e-02-isolation.sh` | **PASS** | T1–T3 + T4 guarded escape suite green on real podman |
| 9 | `scripts/e2e-03-adversarial.sh` | **PASS** | T1–T4 |
| 10 | `scripts/e2e-04-comms.sh` | **PASS** | T1 + T2 ratchets + T3 guarded freight green |
| 11 | `scripts/e2e-05-mayor-adversarial.sh` | **PASS** | T1 + 9 ratchets + T3 manor smoke green (QA05-02 probe executed) |
| 12 | `scripts/e2e-06-guard-mutations.sh` | **PASS** | guard mutation grading |

The single red is a test-harness environment coupling in a file this role may
not edit; everything else — including the container battery that had skipped
since sprint 04 — is green.

### Verify

```sh
TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 ./scripts/ci-local.sh   # full battery (~50 min)
./scripts/guarded-entry.sh --doctor      # why can/can't this user run it
./scripts/village-reclaim.sh --audit     # stranded trees + free space
./scripts/village-reclaim.sh --reclaim   # the only reclaim that works
```

### Carry-forward

- Sprint 06: `guard_acceptance.rs` must SKIP (not panic) without e2e-06's env,
  or `cargo test --workspace` stays red.
- Anyone running guarded suites by hand must go through
  `scripts/guarded-entry.sh` (QA05-09); the lock does not cover a bare
  `cargo test`.
- The e2e-02/04/05 direct routes re-run one suite that step 5 already ran
  (~10 min of the battery). Acceptable for now; a shared "suite already ran in
  this battery" receipt would remove it if the battery keeps growing.

### Verdict

**`GATE 05/05: FAIL`** — on the battery, by exactly one red, and it is not a
product defect:

`cargo test --workspace` fails only in `tests/guard_acceptance.rs`, whose six
tests panic unless `QA_GUARD_SHIM_DIR` / `QA_GUARD_DISK_AREA` are exported —
and only `scripts/e2e-06-guard-mutations.sh` exports them (that harness is
green). The remedy is one `support::skip(...)` in the suite, which this role
may not write. Everything else is green:

- fmt, clippy `-D warnings`, check-deps — clean;
- **the guarded container battery green for the first time in the repo's
  recorded history** — all seven suites ran (12 tests, zero skips), including
  the QA05-02 unshare-write probe in `manor_guarded`, which until now could
  only be declared, never executed;
- all seven `scripts/e2e-*.sh` harnesses green.

Sprint 05's own deliverables are therefore verified end-to-end, and the gate's
long-standing "environmental red" is gone: it was never a host limitation, it
was a battery that asked the wrong user and never asked twice. Re-run
`GATE 05/05` after the `guard_acceptance.rs` skip lands.

---

## Step 05c — ci-agent: GATE 05/05 re-run (full battery, current tree)

Status: **FAIL** — one root cause: an in-flight, uncommitted change set
(ADR-0035 publish-arm seam) leaves `tests/support/mod.rs` uncompilable.
No product defect found by this gate; no product code touched by this step.

### Precondition note

The step-04 line still reads `GATE 05/04: FAIL` (QA re-gate never recorded),
but the defects it names — QA05-04 and QA05-02 — are fixed in **committed**
code (`6c6484a` create-new temp writes; `75e527c` boundary hardening,
ADR-0030) with the finding tests inverted into regressions. This run verifies
the current tree directly instead of trusting that line.

### What ran

`TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 ./scripts/ci-local.sh`
— log: `logs/ci-gate-recheck.log`. Host: podman usable, dock uid 1000,
subuid range OK, btrfs work area with 36 GiB free — the guarded battery ran
for real (no delegation, no skips).

### Gate battery

| # | Check | Result | Notes |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **FAIL** | 5 hunks in 2 in-flight files (below) |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **FAIL** | E0405/E0425 in `tests/support/mod.rs` — missing imports |
| 3 | `cargo test --workspace` | **FAIL** | same compile failure: all seven guarded targets **+ guard_acceptance**, 53 errors each |
| 4 | `scripts/check-deps.sh` | **PASS** | allowlist intact (the in-flight work adds no crate) |
| 5a | village-tree disk policy | **PASS** | stranded trees enumerated; floor met |
| 5 | guarded podman battery | **PASS — vacuous, see CI05-01** | all seven suites green **on stale binaries** (built 08:19, before the 09:15 break) |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | type-enforced boundary |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | T1–T8 |
| 8 | `scripts/e2e-02-isolation.sh` | **FAIL** | T4 rebuilds `escape_guarded` → same compile errors; T1–T3 green (incl. the new `-RP` ratchets) |
| 9 | `scripts/e2e-03-adversarial.sh` | **PASS** | T1–T4 |
| 10 | `scripts/e2e-04-comms.sh` | **FAIL** | T3 rebuilds `freight_guarded` → same compile errors; T1–T2 green |
| 11 | `scripts/e2e-05-mayor-adversarial.sh` | **FAIL** | T3 rebuilds `manor_guarded` → same compile errors; T1 + ratchets green |
| 12 | `scripts/e2e-06-guard-mutations.sh` | **FAIL (refused)** | "refusing to mutate: support/mod.rs is modified in the working tree" — correct behaviour on a dirty tree, derivative of the root cause |
| 13 | `scripts/e2e-07-ci-abort.sh` | **PASS** | T1–T3 |

### Root cause (single)

The ADR-0035 publish-arm seam work is **in flight and uncommitted**: 
`hostperm.rs` (mtime 07:45), `podman.rs` (+82, 07:57), 
`tests/support/mod.rs` (**+356 lines**, 09:15), new untracked 
`docs/adr/0035-publish-arm-injection-seam.md`, `guard_acceptance.rs`, 
e2e-06/07. The runner state shows the owning session died mid-edit 
(`05/3 failed: pi exited with code 1`). The new `PublishSeamRuntime` block
in `tests/support/mod.rs` references types it never imports, so every target
that includes `mod support` fails to build — and the two in-flight files are
also unformatted. Everything else in the battery is green.

Minimal failing output:

```
# [1/5] fmt — 5 hunks, 2 files:
Diff in crates/hamlet-daemon/tests/bootstrap_guarded.rs:980 / :1004 / :1074
Diff in crates/hamlet-daemon/tests/support/mod.rs:468 / :607

# [2/5] + [3/5] clippy / test --workspace — the new block references
# Runtime, Arc, RunSpec, HamletTree, BoxFuture, PodmanError, ExecOutput,
# ExecHandle, SessionExecDuration without importing them:
error[E0405]: cannot find trait `Runtime` in this scope
   --> crates/hamlet-daemon/tests/support/mod.rs:453:34
error[E0425]: cannot find type `BoxFuture` in this scope
   --> crates/hamlet-daemon/tests/support/mod.rs:606:10
error[E0425]: cannot find type `PodmanError` in this scope
   --> crates/hamlet-daemon/tests/support/mod.rs:606:35
error: could not compile `hamlet-daemon` (test "guard_acceptance")
         due to 53 previous errors
# … identical for podman/bootstrap/provision/escape/freight/supervise/manor_guarded

# e2e-06:
refusing to mutate: crates/hamlet-daemon/tests/support/mod.rs is modified in the working tree
```

The fix belongs to the owning role and is small: add the import group to 
`tests/support/mod.rs` (`hamlet_daemon::podman::{Runtime, RunSpec, 
HamletTree, BoxFuture, PodmanError, ExecOutput}` + `std::sync::Arc`) or roll
the change set back; then `cargo fmt`. The step-05b red (guard_acceptance
panicking without e2e-06's env) is **fixed in tree** — the fallback 
`shim_bin()`/`disk_area()` helpers are present — but cannot be exercised
until mod.rs compiles.

### Findings

- **CI05-01 (CI, MEDIUM) — step 5 can read PASS while asserting nothing.**
  The guarded battery's run path never builds: `guarded-service-user.sh`
  resolves "the newest executable" under `target/debug/deps` (its own comment:
  "cargo keeps old hashes around; the newest executable is the current one")
  and the only build attempt sits in `--prepare` behind `|| true`. On a red
  tree it silently runs the last good binary. Measured this run: 
  `escape_guarded-5b9a91b2…` mtime **08:19** (last successful build) vs 
  mod.rs **09:15** (the break) — all seven suites reported green against code
  that no longer compiles. Remedy: the entry point should run 
  `cargo test -p hamlet-daemon --no-run` and hard-fail step 5 on a red build
  (or verify binary freshness against source mtimes). Not applied in this
  step: it would be another uncommitted diff into a tree the owning session is
  mid-edit on, and the verdict is already FAIL on rows 1–3; the hazard only
  misleads if row 5 is read in isolation. Carried with this entry as the record.
- **e2e-06's refusal is correct, not a defect**: mutation grading requires a
  clean tree (it mutates and reverts files). It passes again once the 
  change set lands and formats.

### ADR / provisioning verification (sprint-prompt items)

- **`defaults/MAYOR.md` ships and is referenced by provisioning — YES.**
  Committed (5.6 K, mode 644); embedded via `include_str!` as 
  `MAYOR_MD_TEMPLATE` (`mayor.rs:102`) and seeded into 
  `keep/census/manor/MAYOR.md` by `seed_mayor_md` inside 
  `ManorProvisioner::provision` (`mayor.rs:378`, `416–427`) with adopt-once
  semantics; the golden pins its header.
- **ADR-0011 (host-permission model) matches implementation — YES.**
  This morning's uncommitted amendment (+6/−1) adds the recursive-chown 
  no-dereference clause (`-RP`, manor home only, ADR-0034 cl. 5); the 
  e2e-02 T2 ratchets "the recursive userns chown emits `-RP`" and "no 
  dereference flag anywhere in the userns chown argv" both PASS this run.
  The subuid-1000/0700 clauses are verified by the provision/manor guarded
  suites (green — on stale binaries, but that code is unchanged since 
  step-05b's fresh green), and the unshare-write seam (cl. 4) is intact: 
  `Runtime::unshare_write_atomic` at `podman.rs:1473/1797`. Per the renumber
  note, the prompt's "ADR-0011" = ADR-0026/0027: `cargo test -p hamlet-core 
  --lib` is **160/160 green** (decree vocabulary, replay algebra, packet and
  charter goldens) — the in-flight change set does not touch core.

### Verdict

**`GATE 05/05: FAIL`** — by seven reds with one root cause: the uncommitted 
ADR-0035 change set leaves `tests/support/mod.rs` uncompilable (missing 
import group), which reds fmt/clippy/test-workspace, rebuilds the three 
e2e guarded suites red, and makes e2e-06 refuse. Sprint 05's deliverables
themselves are untouched and remain verified: committed regressions for all
step-04 findings, core goldens 160/160, MAYOR.md shipped and provisioned,
ADR-0011 (incl. this morning's amendment) matching the code.

Remedy (owning role of the ADR-0035 work): finish or roll back the change 
set as above, then re-run GATE 05/05 — `TMPDIR=/var/hamlet/tmp 
HAMLET_PODMAN_TESTS=1 ./scripts/ci-local.sh`.

### Carry-forward

- **CI05-01** (stale-binary vacuous-PASS in step 5) — fix the entry point's 
  build gate before the next sprint's gate; until then, a row-5 PASS is only
  meaningful when rows 1–3 are green.
- The `GATE 05/04` line remains FAIL on paper (QA re-gate never recorded); 
  its named defects are fixed in committed code with inverted regressions. 
  This battery re-verifies them end-to-end once the tree compiles; QA should
  then record the 05/04 re-gate to close the loop.

---

## Step 04b — qa-engineer: sprint 05 button-up (in-flight ADR-0035 change set finished, re-gated)

Status: **done, GATE 05/04 PASS (re-gate recorded); GATE 05/05 green except
e2e-06's clean-tree refusal, which resolves at commit.** Every edit in this
step is test-side (`crates/hamlet-daemon/tests/` + one eprintln reword) —
**no product code was touched**, per role boundary.

### What was blocking the button-up

The ADR-0035 publish-arm change set (test seam + QA04-32 guarded test +
guard-acceptance suite + e2e-06/07 harnesses) was left half-written by a dead
session (step 05c's root cause): `tests/support/mod.rs` referenced types it
never imported, so all seven guarded targets + `guard_acceptance` failed to
compile. Behind the compile break were four more defects in the same in-flight
work, each found and fixed in this step:

1. **`tests/support/mod.rs` — missing import group.** Added
   `Runtime`, `RunSpec`, `BoxFuture`, `PodmanError`, `ExecOutput`, `ExecHandle`,
   `SessionExecSpec`, `ContainerState`, `HamletTree`, `Arc`, `Duration`,
   `PathBuf`. (A first attempt to add a delegating `impl Runtime for Arc<R>`
   here was refused by the orphan rule — the trait is not local to the test
   crate — and withdrawn.)
2. **`tests/bootstrap_guarded.rs` — helpers vs seam-wrapped runtimes.** The
   new QA04-32 test passes `&Arc<PublishSeamRuntime<…>>` where the file's
   `run`/`login` helpers took concrete `&PodmanRuntime`. Made both generic
   over `R: Runtime`; the two Arc-based call sites deref (`&*runtime`).
3. **QA04-32 test — SPEC §5 setup missing.** The test provisioned parcels
   without seeding the charter or writing guild register briefs, so the
   provisioner refused (`no register brief … refusing to seed PROPRIETOR.md`).
   Added the sibling-suite pattern: `Charter::empty().save_atomic` + a brief
   per agent before any provision call.
4. **PublishSeamRuntime — detection written against the wrong argv.** The seam
   matched a bare 3-element `["ln", from, to]`, but every bootstrap exec
   carries the plan's `env K=V …` prefix (bootstrap.rs:1887/2005), so the
   condition could never fire and `rewrites()` stayed 0. Now matches the
   trailing triple (`cmd[len-3] == "ln"`, `to` under `/var/pkgcache/dist/`)
   and preserves the prefix when rewriting to `cp --`.
5. **QA04-32 test — tautological assert.** `!x.is_empty() || x.is_empty()`
   (always true) replaced with what its comment claims:
   `publish_failures.is_empty()`.
6. **`tests/manor_guarded.rs` — stale QA05-02 "LIVE" note.** The probe's
   eprintln still described the pre-step-03b silent-trace failure mode ("will
   fail with only a `decree-ack-write-failed` trace"), which reads in the gate
   log as a live defect. Reworded to the current contract: direct-write EACCES
   is expected on this host shape, and the unshare-seam half (asserted
   immediately below) is what makes it work. Assertions unchanged.

### Verification

Two full batteries plus targeted re-runs; logs `logs/ci-gate-qa-regate.log`
(first run after the compile fix) and `logs/ci-gate-qa-final.log` (final):

| # | Check | Result | Notes |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **PASS** | red once in battery 2 (the seam edit's line-wrap); fixed, re-checked clean on the final tree |
| 2 | clippy `-D warnings`, all targets | **PASS** | clean |
| 3 | `cargo test --workspace` | **PASS** | guard_acceptance 15/15 — SKIP-not-panic holds without e2e-06's env (step-05b red closed) |
| 4 | `scripts/check-deps.sh` | **PASS** | allowlist intact |
| 5a | village-tree disk policy | **PASS** | btrfs work area, floor met |
| 5 | guarded podman battery | **PASS — all seven suites ran**: podman 1, bootstrap **5/5 (QA04-32 green for the first time)**, provision 1, escape 2, freight 1, supervise 1, manor 3 — 14 tests, zero skips |
| 6–11 | e2e-01…05 | **PASS** | e2e-05: T1 17/17 attack tests + 13 ratchets + T3 manor smoke on real podman |
| 12 | e2e-06 | **FAIL (refused)** | "refusing to mutate: support/mod.rs is modified in the working tree" — correct behaviour on a dirty tree; runs once the change set commits |
| 13 | e2e-07 | **PASS** | T1–T3 |

QA04-32's first live run failed twice for reasons above (missing briefs, then
seam never matched); after fixes it ran green end-to-end in both the
single-test re-run (92 s) and battery 2's full bootstrap suite.

### GATE 05/04 re-gate (closing step 04's loop)

**`GATE 05/04: PASS`.** The adversarial suite ran against the final tree in
battery 2: e2e-05 T1 = 17/17 attack tests green, T2 = every ratchet ok
(QA05-04 create-new regression, QA05-02 unshare-seam + surfaced-error
regressions, provenance / quarantine-stability / ack-cadence / source-dedupe),
T3 = manor_guarded 3/3 on real podman with the QA05-02 probe **LIVE** on this
host (direct write EACCES) and its seam half succeeding (bytes arrive through
`unshare_write_atomic`, ownership to the Mayor slot). Both HIGH findings
remain fixed with inverted regressions.

### Findings

No product defects found in this button-up. The ADR-0035 change set's
product-side half — the recursive userns chown argv `-R` → `-RP` (ADR-0034
cl. 5, QA04-29) — is pinned by `chown_recursive_argv_pins_no_dereference`
and both e2e-02 T2 ratchets, and green in this battery. The seam itself is
honest about scope (simulates shared-device topology; does not fake the
product's `ln`).

### Carry-forward (CI / commit step)

- **After the ADR-0035 change set commits, re-run
  `scripts/e2e-06-guard-mutations.sh`** (or the full battery): its refusal is
  only against a dirty `support/mod.rs`, and on a clean tree it runs mutation
  grading — T1's guard_acceptance is already 15/15 green, so PASS is the
  expected outcome. That single re-run flips **GATE 05/05: PASS** (current
  state: FAIL on that one row only).
- Untracked items **not** part of the ADR-0035 change set — do not sweep
  into its commit: `assets/` (game art), `pi-session-2026-10-04….html`
  (session export), `roadmap/.runner-state.json` (runner state).
- **CI05-01** (stale-binary vacuous-PASS in step 5) remains the CI agent's
  carry-forward, unchanged.

---

## Step 05d — ci-agent: GATE 05/05 re-run (post-button-up battery, current tree)

Status: **FAIL** — by exactly one red: `e2e-06`'s clean-tree refusal, whose
root cause is the still-uncommitted ADR-0035 change set (commit-step scope).
No product defect found; no product code and no CI config touched by this
step.

### Precondition

`GATE 05/04: PASS` — recorded in step 04b (qa button-up). Met; the gate runs.

### What ran

`TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 ./scripts/ci-local.sh`
— log: `logs/ci-gate-ci5-rerun.log`. Host: podman 6.1.3 usable, dock uid 1000,
subuid range OK — the guarded battery ran directly, no delegation, no skips.
Pre-run: reclaimed **33 stranded village trees** from the work area
(`village-reclaim.sh --reclaim`), free space 16G → 37G; two `hamlet`-owned
trees remain (owner-only reclaim — needs `sudo -H -u hamlet`, non-blocking;
the 4 GiB QA04-08 floor is met).

Freshness (CI05-01 discipline): the guarded binaries were built **after** the
newest test-source edit (binaries 10-06 00:04 vs `support/mod.rs` 10-05
21:00), and steps 1–3 re-verified the current tree compiles clean with zero
recompilation — so row 5 is a real read of this tree, not a stale-binary
vacuous PASS.

### Gate battery

| # | Check | Result | Notes |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **PASS** | clean |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | clean |
| 3 | `cargo test --workspace` | **PASS** | incl. `guard_acceptance` 15/15 — SKIP-not-panic holds without e2e-06's env (step-05b red stays closed) |
| 4 | `scripts/check-deps.sh` | **PASS** | allowlist intact |
| 5a | village-tree disk policy | **PASS** | 33 stranded trees reclaimed pre-run; floor met |
| 5 | guarded podman battery | **PASS — all seven suites ran**: podman 1, bootstrap 5 (QA04-32), provision 1, escape 2, freight 1, supervise 1, manor 3 — 14 tests, zero skips |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | type-enforced boundary |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | T1–T8 |
| 8 | `scripts/e2e-02-isolation.sh` | **PASS** | T1–T4 (incl. `-RP` ratchets, guarded escape on real podman) |
| 9 | `scripts/e2e-03-adversarial.sh` | **PASS** | T1–T4 |
| 10 | `scripts/e2e-04-comms.sh` | **PASS** | T1–T3 (guarded freight green) |
| 11 | `scripts/e2e-05-mayor-adversarial.sh` | **PASS** | T1 17/17 attack tests + all T2 ratchets + **T3 manor provision smoke 3/3 on real podman** (UID-1000-only decree authorship; QA05-02 probe LIVE) |
| 12 | `scripts/e2e-06-guard-mutations.sh` | **FAIL** | clean-tree refusal — see below |
| 13 | `scripts/e2e-07-ci-abort.sh` | **PASS** | T1–T3 |

### The single red (minimal output)

```
==> [e2e] scripts/e2e-06-guard-mutations.sh
refusing to mutate: crates/hamlet-daemon/tests/support/mod.rs is modified in the working tree
  FAIL [e2e] scripts/e2e-06-guard-mutations.sh
```

Root cause: the ADR-0035 change set (publish-arm test seam + QA04-32 +
`guard_acceptance` + e2e-06/07, plus the `-RP` chown amendment) is still
**uncommitted**. `e2e-06` refuses to mutate a dirty `support/mod.rs` so its
mutation grading can never eat in-flight work — correct harness behaviour,
and exactly the state step 04b predicted ("resolves at commit"). Every other
row ran against this same tree and is green; the refusal asserts nothing, so
it is not a coverage pass either.

### ADR / provisioning verification (sprint-prompt items)

- **`defaults/MAYOR.md` ships and is referenced by provisioning — YES.**
  Committed (`bd171c2`, mode 644, 5.6 K); embedded via `include_str!` as
  `MAYOR_MD_TEMPLATE` (`mayor.rs:102`) and seeded into the manor home by
  `seed_mayor_md` inside `ManorProvisioner::provision` (`mayor.rs:378`,
  `416–427`) with adopt-once semantics; the golden pins its header.
- **ADR-0011 (host-permission model) matches implementation — YES.**
  cl. 1: no `--userns` on any run argv, `--user <uid>` present (golden pinned
  at `podman.rs:2600`); cl. 2: the recursive userns chown emits `-RP`, never a
  dereference flag (`hostperm.rs:389`; e2e-02 T2 ratchets green this run);
  cl. 3: the host mode table as constants (parcel home / mail spool 0700,
  mail root 0755, pkgcache 01777 — `hostperm.rs`); cl. 4: daemon writes into
  subuid-owned trees through `Runtime::unshare_write_atomic`
  (`podman.rs:1473`) — exercised LIVE by the QA05-02 probe in `manor_guarded`
  (3/3 green this run).

### Verdict

**`GATE 05/05: FAIL`** — by exactly one red, and it is not a product defect:
`e2e-06` refuses to mutation-grade on the uncommitted ADR-0035 change set.
Everything else is green, including:

- fmt, clippy `-D warnings`, check-deps — clean;
- `cargo test --workspace` fully green (incl. `guard_acceptance` 15/15);
- the guarded container battery — all seven suites ran on real podman
  (14 tests, zero skips), incl. the manor provision smoke and the QA05-02
  unshare-write probe LIVE;
- all other e2e harnesses green.

Sprint 05's deliverables remain verified end-to-end; the gate's only red is a
harness doing its job on a dirty tree.

### Remedy / carry-forward

- **Commit the ADR-0035 change set (step 06 — commit agent), then re-run
  `scripts/e2e-06-guard-mutations.sh`** (or the full battery). Step 04b's
  carry-forward expects that single re-run to flip this gate: on a clean tree
  e2e-06 runs mutation grading, and its T1 (`guard_acceptance`) is already
  15/15 green in row 3 of this very battery. Do not sweep the untracked
  non-change-set items into that commit (`assets/`, the session-export html,
  `roadmap/.runner-state.json`) — step 04b's note stands.
- **CI05-01** (stale-binary vacuous-PASS in step 5) remains the CI agent's
  carry-forward. Its hazard did not materialize this run (freshness verified
  above); the entry-point build gate is still owed before the next sprint's
  gate.
- The two `hamlet`-owned stranded trees under `/var/hamlet/tmp` need their
  owner: `sudo -H -u hamlet podman unshare rm -rf <tree>` (operator action,
  non-blocking).

---

## Step 06 — commit-agent: ADR-0035 change set committed; GATE 05/05 re-run (clean tree)

Status: **PASS** — the change set is committed, and the full battery is
green on the clean tree.

### Commits (`sprint/05-mayor-loop`, in order)

| SHA | Scope | Why |
| --- | ----- | --- |
| 3e48a32 | `fix(daemon)` | ADR-0034 cl. 5 + cl. 6 — the recursive chown brace is pinned to `-RP` (argv policy, hermetic + guarded pins, test-side reclaim helpers) |
| 581e2b5 | `test(daemon)` | QA04-26/27 — per-run container names, cold-bootstrap serialisation, dead-parcel SKIP classification |
| 558d886 | `test(daemon)` | ADR-0035 — forced-warm mirror arm (`PublishSeamRuntime` + the guarded warm-arm test) |
| a44f437 | `test(daemon)` | QA04-30 — guard acceptance suite + e2e-06 mutation harness |
| 87eaf08 | `ci` | guarded battery entry point, disk policy, abort verdicts (QA04-31, QA05-09/CI05) |
| 76e3865 | `docs(roadmap)` | sprint 04 + 05 HANDOFF records |
| 6a41b29 | `test(daemon)` | QA05-11 — latency-spike timing margin (below) |

Untracked non-change-set items remain untracked, per step 04b: `assets/`,
the session-export html, `roadmap/.runner-state.json`.

### The one red of the first post-commit run — and its fix (QA05-11)

The first full-battery re-run after committing was red by exactly one line,
and it was not in the change set: `metrics.rs::
latency_spikes_follow_the_configured_threshold` read the scripted 120 ms tool
call back as **97 ms** against its `>= 100` floor — under battery load (it
passed standalone and in the earlier DoD run of the same code). Spans are
measured between frame *reads*, so the shortfall is host-load-dependent; a
gate must not be a coin flip. The scripted delay now runs **300 ms** with the
assertions unchanged (`6a41b29`): the floor still catches a collapsed span
while no realistic load can sink it.

### GATE 05/05 re-run (clean tree)

**`GATE 05/05: PASS`.** `TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1
./scripts/ci-local.sh`, exit 0:

- fmt, clippy `-D warnings`, check-deps — clean;
- `cargo test --workspace` fully green (incl. `guard_acceptance` 15/15);
- the guarded container battery — all seven suites ran on real podman;
- **e2e-06 mutation grading PASS on the clean tree** — the step 05d red is
  gone;
- e2e-07 ci-abort regression PASS; all other harnesses green.

Step 06's precondition (HANDOFF shows `GATE 05/05: PASS`) is met; the merge
& tag follow per `06-commit-agent.md`.

### Merge & tag

- **Squash-merged** `sprint/05-mayor-loop` → `main`: **`51dbfa9`** (21 branch
  commits squashed; 65 files, +21199). Tag **`sprint/05-mayor-loop`** →
  `51dbfa9`.
- Battery on merged main: the first run was red by one line — e2e-06's
  hermetic work-area test needs ≥ 4 GiB free in the *real* area, and
  repeated battery runs had stranded `.tmp*` village trees down to 2.5 GB
  (host shape, not a defect: the guard refused correctly, and e2e-05's manor
  smoke SKIPped with its recorded QA04-08 remedy). After the operator
  reclaimed the stranded trees (~27 GiB free restored): **full battery PASS
  on `main`** (`TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1
  ./scripts/ci-local.sh`, exit 0) — GATE 05/05 holds on the merged artifact.

---

## Step 05e — ci-agent: GATE 05/05 final verification (post-merge main, tagged artifact)

Status: **PASS** — the full battery is green on the tagged artifact with zero
skips. Precondition met: `GATE 05/04: PASS` (recorded in step 04b). No product
code touched by this step; no CI config changed either (nothing needed fixing).

### What ran

Tree: `main` @ `a68143e` — the docs commit on top of squash-merge `51dbfa9`
(tag `sprint/05-mayor-loop`). Working tree clean except the three known
untracked non-change-set items (`assets/`, the session-export html,
`roadmap/.runner-state.json`), left untouched.

`TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 ./scripts/ci-local.sh`
— log: `logs/ci-gate-postmerge.log`, exit 0 (`CI battery: PASS`).
Host shape: podman 6.1.3 rootless, dock uid 1000, subuid range 65536 ids,
btrfs work area (17.3 GiB free at the 5a audit) — the guarded battery ran
**directly** through `guarded-entry.sh` (no delegation, no skips).

### Gate battery

| # | Check | Result | Notes |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **PASS** | clean |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | clean |
| 3 | `cargo test --workspace` | **PASS** | 785 tests ok across all targets; core lib 160/160 (decree + packet goldens = ADR-0026/0027); `guard_acceptance` 15/15 — SKIP-not-panic holds without e2e-06's env (step-05b red stays closed) |
| 4 | `scripts/check-deps.sh` | **PASS** | allowlist intact |
| 5a | village-tree disk policy | **PASS** | survey at 17.3 GiB free; 14 pre-existing stranded trees enumerated, floor met |
| 5 | guarded podman battery | **PASS — all seven suites ran**: podman 1, bootstrap 5 (QA04-32), provision 1, escape 2, freight 1, supervise 1, manor 3 — 14 tests, zero skips; QA05-02 probe executed LIVE (direct write EACCES as expected on this host shape; bytes arrive through the unshare seam) |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | type-enforced boundary |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | T1–T8 |
| 8 | `scripts/e2e-02-isolation.sh` | **PASS** | T1–T4 incl. the `-RP` ratchets and the guarded escape suite on real podman |
| 9 | `scripts/e2e-03-adversarial.sh` | **PASS** | T1–T4 |
| 10 | `scripts/e2e-04-comms.sh` | **PASS** | T1–T3, guarded freight green |
| 11 | `scripts/e2e-05-mayor-adversarial.sh` | **PASS** | T1 = 17/17 attack tests + all T2 ratchets + **T3 manor provision smoke on real podman** (UID-1000-only decree authorship; QA05-02 probe LIVE) |
| 12 | `scripts/e2e-06-guard-mutations.sh` | **PASS** | mutation grading ran on the clean tree — the step-05d refusal is gone. The one `test result: FAILED (14 passed; 1 failed)` line in the log is a **killed mutation** (the acceptance suite went RED against an injected change, as designed) and was reverted: `git status` clean afterwards |
| 13 | `scripts/e2e-07-ci-abort.sh` | **PASS** | T1–T3 abort regressions |

### CI05-01 freshness (stale-binary discipline)

Verified for this run: step 3 rebuilt every test target from the committed
tree **before** step 5 ran (the `escape_guarded` binary used by the battery was
built at 15:45, inside this battery; the e2e-phase rebuilds all post-date
step 5). Row 5 is therefore a real read of this tree, not a stale-binary
vacuous PASS. The entry-point build gate itself remains owed (carry-forward).

### ADR / provisioning verification (sprint-prompt items)

- **ADR-0011 (host-permission model) matches implementation — YES.**
  cl. 1: `--user <uid>` on every run argv, no `--userns` flag
  (`podman.rs:586–588`; run-argv golden pinned); cl. 2: per-UID ownership via
  `podman unshare chown <uid>:<gid> -- <paths…>` with the recursive form
  pinned to `-RP`, never a dereference flag (`podman.rs:891`;
  `hostperm.rs:389`; e2e-02 T2 ratchets green this run); cl. 3: the host mode
  table as constants (`hostperm.rs:29–58`) — the freight row is amended by
  **ADR-0021 cl. 4** to `0o1770` (amendment recorded there; code matches the
  amended value); cl. 4: daemon writes into subuid-owned trees through
  `Runtime::unshare_write_atomic` (`podman.rs:1473/1797`) — exercised LIVE by
  the QA05-02 probe in `manor_guarded` this run.
- **Per the renumber note, "ADR-0011" also = ADR-0026/0027** (decree protocol,
  context packet): core goldens 160/160 green in row 3 — decree vocabulary +
  replay algebra and the packet schema/caps all verified.
- **`defaults/MAYOR.md` ships and is referenced by provisioning — YES.**
  Committed in `51dbfa9` (mode 644, 5.6 K); embedded via `include_str!` as
  `MAYOR_MD_TEMPLATE` (`mayor.rs:102`) and seeded into the manor home by
  `seed_mayor_md` inside `ManorProvisioner::provision`
  (`mayor.rs:378`, `416–427`) with adopt-once semantics; the golden pins its
  header, and the T3 manor smoke provisions it in a real container this run.

### Observations (CI-side; no product defects)

- **Orphaned guarded artifact removed.** Container `hamlet-boot-a-r1939730`
  had been Up 21 h with no owning process (leaked by an earlier aborted run —
  its rollback did not finish). Removed (`SIGKILL` after the SIGTERM timeout);
  its tree (`.tmpCf3Uom`) was reclaimed in the sweep below. This is the QA05-09
  rule made concrete: hand-run guarded work bypasses `guarded-entry.sh`'s lock
  and can leak; the battery's own rollbacks cleaned up everything this run
  created.
- **Disk hygiene after the gate.** A fully green battery still strands trees:
  49 total under `/var/hamlet/tmp` after the run (~14 pre-existing + ~35 from
  this run, ~9 GB net) — same accumulation step 06 measured. Ran
  `scripts/village-reclaim.sh --reclaim`: free space **8.3 G → 39 G**;
  two 32 K residuals remain (one `hamlet`-owned, `.tmpyEhz0K`, owner-only:
  `sudo -H -u hamlet podman unshare rm -rf <tree>` — negligible).
- **Carry-forward (CI):** the 5a policy audits *before* each gate and reclaims
  only on abort; post-run reclaim is still operator action. If the work area
  keeps shrinking, add an automatic `--reclaim` step after the battery in
  `ci.sh` — a CI-config change this role may make, proposed rather than made
  here so the gated tree stays untouched.
- **CI05-01** (entry-point build gate) remains the standing carry-forward,
  unchanged; its hazard did not materialize this run (freshness verified above).

### Verdict

**`GATE 05/05: PASS`** — all fourteen rows green on the tagged artifact
(`main` @ `a68143e`, tag `sprint/05-mayor-loop`), zero skips, exit 0; the
guarded container battery ran for real (14 tests incl. the manor provision
smoke and the QA05-02 unshare-write probe LIVE); ADR-0011 (incl. the
ADR-0021 freight amendment) matches the implementation and `defaults/MAYOR.md`
ships and is provisioned. Sprint 05 is verified end-to-end on the merged
artifact; nothing outstanding blocks the tag.

---

# Sprint 06 — Commission Lifecycle

## Step 01 — architect: commission state machine, git mediation schema, promotion parsers

Status: **done**. Commit:
`feat(core): commission state machine, git mediation schema, promotion
parsers (ADR-0012)`. The prompt cites "ADR-0012", which is the derived-image
ADR (Sprint 02); per the `docs/adr/README.md` renumber rule this work is
**ADR-0036** (commission state machine + charter v4 + INDENTURE grammar),
**ADR-0037** (git push mediation) and **ADR-0038** (DEED alteration grammar +
promotion request). The README index and renumber table were updated in the
same commit; later sprint-06 prompts reading "ADR-0012" must read
0036/0037/0038.

### Done

- **`hamlet-core::commission`** (SPEC §9.1–9.3; ADR-0036) — the typed state
  machine `Draft → Active → WindingDown → Closed` (+ `Failed`). The complete
  `(state, event)` table is defined: all 5×7 pairs enumerated, illegal ones a
  typed `IllegalTransition` (from/to/event), never a panic. Events:
  `IndenturePosted`, `AllEntriesDone`, `DeliverableSubmitted`,
  `PromotionApproved`, `PromotionRejected`, `OperatorClose`,
  `EscalationFatal`. `CommissionRecord` = `{slug, state, roster,
  indenture_entries, deliverable_in_review}` — canonical sorted-key JSON; ops:
  `draft(slug)`, `apply_event`, `assign_uid`/`release_uid` (roster open in
  Draft/Active only), `mark_entry` (active-only, forward-only over
  `IndentureEntryStatus`).
- **INDENTURE.md grammar** (ADR-0036 cl. 3) — strict, LF-only: line 1
  `# INDENTURE`; blocks `## <agent>` (SafeName), each with exactly three
  bullets in order `- goal:`, `- scope:`, `- done:`; blank lines between
  blocks allowed, nothing else legal; ≥ 1 block; file ≤ 32 KiB, field ≤ 512 B.
  `IndentureDoc::parse`/`render` round-trips byte-exact; every error carries
  the offending line.
- **Charter v3 → v4** (in-place retype) — `commissions` values become the
  per-commission `CommissionRecord` object (was a string status); the charter
  is the single source of truth for commission state. Load-time invariants:
  rostered uid ⇒ slot exists and is bound; ids ≥ 1. Wire leniency: untagged
  enum with the legacy strings first — v3 files load and migrate
  (`open` → Draft, `closed` → Closed, bijective). New ops API on `Charter`:
  `next_commission_id`, `create_commission`, `ensure_commission`,
  `assign_uid`, `release_uid`, `apply_commission_event`,
  `mark_indenture_entry`. The state grid (ADR-0027) now carries the full
  record.
- **`hamlet-core::gitmed`** (SPEC §14 [DEV]; ADR-0037) — the frozen mediation
  schema: spool = the agent's own home (`tree::parcel_git_spool(uid)` →
  `keep/census/parcel<uid>/home/git-spool/`); candidate names
  `^[A-Za-z0-9._-]{1,128}\.bundle$`; bundle cap 32 MiB; per-agent branch
  `agent/<uid>` (ref `refs/heads/agent/<uid>`); the seeded identity
  `<uid>@hamlet.local` must be author **and** committer; typed rejection
  vocabulary (`oversize/verify/ref/identity/nonff`) with stable tokens for
  refusal receipts. The executor (verification ladder, object graft, ref
  update, archive/consume, `GitDiffReady` wake) is daemon-side — step 04.
- **`hamlet-core::promote`** (SPEC §9.4; ADR-0038) — the strict DEED
  `### Dynamic Alterations` parser: at most one section (zero = no-op, two =
  error); lines `- <kind> <name>[ = <version_spec>]`, kind ∈ {pixi, nvm,
  cargo, apk}, names/specs reusing the blueprint's own validators; line-
  numbered errors. `apply_alterations` upserts onto the `[tools]` schema
  (replace version_spec, append new in declaration order, bump
  `meta.version`, refused at u32::MAX) and returns a **fresh immutable**
  `Blueprint`. `PromotionRequest` = `{commission, freight_path, artifact_name}`
  with validation mirroring the decree-side freight rules.
- **Ripples** — `tree.rs`: `parcel_git_spool(uid)` accessor; `watch.rs`:
  `Reaction::GitBundle { uid, name }` row (spool Created files only);
  `blueprint.rs`: internal constructors (`ToolSpec::new`,
  `with_version_spec`, `BlueprintMeta::bumped_version`, `Blueprint::assemble`)
  + `BlueprintError::VersionOverflow`; the `CommissionStatus` string enum is
  gone from the public API; ripple fixes across charter/packet/census/
  freight/goldens (incl. the CLI init golden v3→v4 and the e2e-01 T1 golden).

### Decisions (ADR refs)

- **Legacy mapping bijective and conservative**: `open` → Draft, `closed` →
  Closed. File-as-truth: no INDENTURE.md = not decomposed = Draft; there is
  no way to guess Active out of a v3 file.
- **The repo path is not stored** — derived via the existing
  `tree::commission_git(n)` accessor (one home per fact).
- **Roster ops legal in {Draft, Active} only**; WindingDown+ freezes the
  roster. Entry marking is active-only and forward-only.
- **Git mediation keeps agents on their rw mount**: they stage bundles into
  their own home spool; the daemon is the sole validator/applier. No git
  hooks (invisible to the contract), branch-per-agent, non-fast-forwards
  refused with a receipt, conflicts archived + refused rather than merged by
  guesswork. `GitDiffReady` carries the diff body up to 64 KiB; above it
  degrades to the deterministic `--stat` summary — never a truncated diff.
- **DEED promotion is replacement, not mutation**: alterations produce a new
  immutable Blueprint (ADR-0009 immutability holds); vault copies are
  daemon-performed in step 06; `PromotionRequest` is transient executor input,
  never charter state.

### ADR cap deviation (disclosed)

`docs/adr/README.md` caps ADR bodies at 40 lines. The three ADRs this step
lands are dense normative schemas: **ADR-0036 = 78 body lines, ADR-0037 = 75,
ADR-0038 = 62**. Compressing them would drop the normative detail steps
02–06 build against (transition tables, validation ladder, grammars).
Precedent: ADR-0030 shipped at ~70 with disclosure. The split that was worth
making was made (state machine / git mediation / DEED promotion); splitting
further would fragment coupled schemas.

### Verify

```sh
cargo test -p hamlet-core --lib commission   # 18: full transition table, record ops, INDENTURE parse/render + refusals
cargo test -p hamlet-core --lib gitmed       # 4: naming, bundle-name grammar, rejection tokens, thresholds
cargo test -p hamlet-core --lib promote      # 13: DEED grammar matrix, upsert semantics, PromotionRequest validation
cargo test -p hamlet-core --lib charter      # 19: v3→v4 migration + invariants, ops API, wire leniency
cargo test -p hamlet-core --lib watch        # 6: GitBundle row (spool Created files only)
cargo test -p hamlet-core --lib tree         # 8: parcel_git_spool accessor
./scripts/ci-local.sh                        # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…07
```

Workspace battery green (195 core-lib unit tests + all daemon/CLI suites).
`ci-local.sh`: **CI battery: PASS** with the one standing **environmental**
skip — `[5/5] guarded podman battery not runnable on this host` (pre-existing
QA04-07/08/09 blockers: no dedicated service user, `/tmp` tmpfs work area).

### Open issues / carry-forward

- **Step 02 (daemon)** consumes the charter ops API: `create_commission` +
  the `CommissionCreated` wake wiring, INDENTURE.md seeding at provisioning,
  and the spool accessor. Core-side is ready; no daemon plane calls these yet.
- **Steps 03/04 (daemon)**: the git bundle executor per ADR-0037
  (verification ladder, object graft, ref update, archive/consume + refusal
  receipts, `GitDiffReady` wake) and the promotion executor per ADR-0038
  (vault copy, DEED write-back via `apply_alterations`, decree acks). Both
  daemon-side by design (purity seam).
- The commission-lifecycle e2e script is a later step of this sprint; the
  exit tag `sprint/06-commission-lifecycle` waits on the full battery.

---

## Step 02 — daemon-engineer: commission creation, bare repos, CLI basics

Status: **done**. Commit: `feat(daemon,cli): commission creation + bare git repos (SPEC §9.1)`.
Consumes **ADR-0036** (commission state machine + charter v4), **ADR-0037**
(bare git repo passive storage, no server hooks), **ADR-0011** (host
permission model), and **ADR-0007** (symlink refusal + `create_new` writes).

### Done

- **`hamlet_daemon::ledger`** (`crates/hamlet-daemon/src/ledger.rs`) — the
  guild ledger governor:
  - `Ledger::create_commission(raw_slug, brief, seed_dir)`:
    1. Sanitizes `<slug>` via `hamlet_core::sanitize::sanitize_name`.
    2. Validates `brief` before any side effect (non-empty, LF-only — CR bytes
       rejected per ADR-0019 cl. 1 — and ≤ `MAX_ARTIFACT_BYTES`).
    3. Validates optional `--seed <dir>` before any side effect: `symlink_metadata`
       must be a real directory; walks recursively in sorted order refusing
       symlinks and non-regular files anywhere in the seed tree (ADR-0007), while
       ignoring an existing `.git/` directory if the user seeds from a checkout.
    4. Allocates the next sequential 1-based commission ID via
       `Charter::create_commission` in `Draft` state (ADR-0036 cl. 1–2:
       file-as-truth — `Draft` transitions to `Active` when `INDENTURE.md` is
       posted in Step 03).
    5. Creates `guild/ledger/commission<n>/` and initializes a bare git
       repository at `guild/ledger/commission<n>/git/` (`HEAD ->
       refs/heads/main`, `core.hooksPath = /dev/null` per ADR-0037 cl. 4,
       world-readable `0755`/`0644` modes per ADR-0011).
    6. When `--seed <dir>` is given, stages validated regular files into an
       isolated temporary worktree/index and creates the root commit on
       `refs/heads/main` via `git read-tree --empty` + `git add` +
       `git write-tree` + `git commit-tree` + `git update-ref`, authored by
       `hamlet-daemon <daemon@hamlet.local>` with a `TITLE`-style lineage body
       (`seed_commit_message`).
    7. Writes `guild/ledger/commission<n>/COMMISSION.md` via `create_new`
       (`0644`, ADR-0007).
    8. Saves `charter.json` atomically last (`Charter::save_atomic`); any
       failure prior to charter commit removes `guild/ledger/commission<n>/`.
    9. Builds `WakeEvent::from_commission(&tree, n)` (`WakeReason::CommissionCreated`)
       and fires the `CommissionWake` hook (`NoopCommissionWake`,
       `RecordingCommissionWake` spy, `CommissionWakeBridge` for `MayorEngine`).
  - **Roster management API**: `Ledger::assign_uid`, `Ledger::release_uid`,
    `Ledger::roster`, and `Ledger::apply_event` persist atomically to
    `charter.json` and feed Sprint 04 freight ACL policy
    (`FreightGuard::policy` / `FreightGuard::policy_for_commission`), Sprint 05
    `DecreeExecutor` target validation (`ReassignTask` verifies commission
    liveness and roster membership), and `Provisioner::provision_steps`
    (auto-rostering newly provisioned slots onto live commissions).
  - **Tree-only query views**: `Ledger::list_commissions` (`render_commission_list`)
    and `Ledger::show_commission` (`CommissionDetail::render`) derive output
    strictly from `charter.json` and `.hamlet/guild/ledger/commission<n>/` with
    zero wall-clock fields.
- **`hamlet-cli::commission`** (`crates/hamlet-cli/src/commission.rs` +
  `main.rs`):
  - `hamlet commission new <slug> --brief <file|-> [--seed <dir>] [--dir <d>]`
  - `hamlet commission list [--dir <d>]`
  - `hamlet commission show <slug> [--dir <d>]`
  - Usage errors exit `2`; validation/ledger/symlink errors exit `1`.
- **Charter `CommissionWire` deserialization fix** (`crates/hamlet-core/src/charter.rs`):
  replaced `#[serde(untagged)]` on `CommissionWire` with a direct `Visitor` so
  non-empty `indenture_entries: BTreeMap<u32, IndentureEntryStatus>` preserves
  `serde_json`'s integer map-key coercion across `Charter::save_atomic` /
  `Charter::load`.

### Decisions

- **`Draft` on creation, `Active` on `IndenturePosted` (ADR-0036 cl. 1–2)**:
  although the Step 02 prompt parenthetically wrote `(Draft → Active on
  creation)`, ADR-0036 (ratified in Step 01 and binding per CONVENTIONS §10)
  defines `Created` as constructor-only into `Draft` and requires a non-empty
  `IndenturePosted` entry set `⊆ roster` to enter `Active`. Step 02 therefore
  registers new commissions in `Draft`, ready for Step 03's `INDENTURE.md`
  watcher to fire `IndenturePosted`.
- **Plumbing-based `--seed` import (`write-tree` + `commit-tree` + `update-ref`)**:
  avoids worktree locks on a `core.bare = true` repository, never touches
  parent git repositories or host git config (`GIT_CONFIG_NOSYSTEM=1`), and
  cleans up staging files before `COMMISSION.md` and `charter.json` are
  committed.
- **Wall-clock-free CLI summaries (CONVENTIONS §3 r4)**: commit timestamps live
  only inside the git commit object and `TITLE`-style commit message; CLI
  `new`, `list`, and `show` outputs report structural facts (`main: unborn` vs
  `main: seeded`, file counts) so golden comparisons are byte-exact across runs.

### Verify

```sh
cargo test -p hamlet-core --lib charter            # 20: + active_commission_with_indenture_entries_roundtrips_json
cargo test -p hamlet-daemon --test ledger          # 8 integration tests (no podman):
                                                   #   creation flow + bare git + COMMISSION.md + wake spy,
                                                   #   CommissionWakeBridge -> MayorEngine single-turn wake,
                                                   #   slug sanitization + brief/duplicate rejections,
                                                   #   --seed import + TITLE lineage + symlink refusal,
                                                   #   roster management -> FreightGuard::policy + DecreeExecutor,
                                                   #   multi-tenancy (2 commissions, disjoint rosters, independent
                                                   #     states, independent git dirs, distinct MountSet::for_agent),
                                                   #   restart resumability from charter.json + tree alone
cargo test -p hamlet-cli --test commission_golden  # 2: new/list/show byte-exact goldens + bad-slug/symlink/usage exits
./scripts/ci-local.sh                              # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…05
```

### Open issues / carry-forward

- **Step 03 (daemon)**: watch `Reaction::ReconcileCommission` on
  `guild/ledger/commission<n>/INDENTURE.md`, parse `IndentureDoc`, transition
  `Draft → Active` via `Ledger::apply_event(n, &CommissionEvent::IndenturePosted { .. })`,
  and route tasks into rostered agent sessions.
- **Step 04 (daemon)**: git bundle mediation executor per ADR-0037 against the
  bare repository initialized here (`guild/ledger/commission<n>/git/`).

---

## Step 03 — daemon-engineer: INDENTURE decomposition + entry tracking

Status: **done**. Commit: `feat(daemon): INDENTURE decomposition + entry tracking (SPEC §9.2–§9.3)`.
Consumes **ADR-0036** (`IndentureDoc`, `IndentureEntryStatus`, `CommissionEvent::IndenturePosted` / `AllEntriesDone` / `EscalationFatal`), **ADR-0026/0029** (`DecreeExecutor`, `EscalationNotice`), **ADR-0027** (`ContextPacket` / `WakeReason`), **ADR-0019** (`MailRouter`), and **ADR-0007** (`create_new` atomic writes, symlink refusal).

### Done

- **`hamlet-core` addenda**:
  - `tree::HamletTree::commission_indenture_ack(n)` (`guild/ledger/commission<n>/INDENTURE.md.ack`).
  - `commission::{IndentureAck, IndentureAckStatus, INDENTURE_ACK_VERSION, parse_entry_notice_subject}`: canonical sorted-key JSON `.ack` receipt (`done` / `rejected` / `escalated`, `attempt`, FNV-1a/64 `digest`, optional `error`) and strict `Subject: commission:<n>:done` / `commission:<n>:in_progress` parser.
  - `packet::{WakeReason, EventKind}`: added `WakeReason::IndentureRejected` (`"indenture_rejected"`) and `WakeReason::AllEntriesDone` (`"all_entries_done"`), plus `EventKind::IndentureRejected { commission, attempt }` and `EventKind::AllEntriesDone { commission }`.
- **INDENTURE reconciliation & per-agent task delivery (`crates/hamlet-daemon/src/ledger.rs`, `decrees.rs`)**:
  - `DecreeExecutor::scan()` executes `ProvisionAgent` decrees (allocating slots, provisioning parcels, starting sessions with `agent_commission_brief_addendum`, and rostering each UID on `commission<n>`) before calling `Ledger::reconcile_indentures()` at the end of the pass.
  - `Ledger::reconcile_indenture(n)` validates `guild/ledger/commission<n>/INDENTURE.md` via `IndentureDoc::parse` and verifies every `## <agent>` block resolves to a provisioned slot rostered on `commission<n>`.
  - **On valid `INDENTURE.md`**: transitions `commission<n>` `Draft → Active` via `CommissionEvent::IndenturePosted { entries }`, writes a `done` `.ack` at `INDENTURE.md.ack`, idempotently appends `## Commission <n> Indenture` (`goal`, `scope`, `done`, and completion instructions) to `parcel<uid>/PROPRIETOR.md`, sends each rostered agent a daemon-authored (`From: system`) indenture mail (`Subject: commission:<n>:indenture (<slug>)`) with commission context refs (`COMMISSION.md`, `INDENTURE.md`, `/var/git`, `/var/freight/`), and marks each delivered entry `IndentureEntryStatus::Delivered` in `charter.json`.
  - **On invalid `INDENTURE.md`**: quarantines `INDENTURE.md` to `.indenture.rejected`, writes a `rejected` `.ack` recording `attempt` and line-numbered error details, and wakes the Mayor with `WakeReason::IndentureRejected` (`WakeEvent::from_indenture_rejected`).
  - **Bounded retry escalation**: after `DEFAULT_MAX_INDENTURE_RETRIES` (3, configurable via `Ledger::with_max_indenture_retries`), transitions `commission<n>` `Draft → Failed` (`CommissionEvent::EscalationFatal`), writes an `escalated` `.ack`, and files an `EscalationNotice` under `keep/census/manor/escalations/`.
- **Entry completion signalling (`crates/hamlet-daemon/src/mailroute.rs`, `ledger.rs`, `defaults/MAYOR.md`)**:
  - Documented the completion notice convention (`To: 1000`, `Subject: commission:<n>:in_progress` / `Subject: commission:<n>:done`) in `defaults/MAYOR.md`, in the worker session brief addendum (`agent_commission_brief_addendum`), and in the `PROPRIETOR.md` / indenture mail instructions.
  - `MailRouter::deliver` intercepts slot-authored mail to `MAYOR_UID` matching `parse_entry_notice_subject` and invokes `Ledger::handle_entry_notice` (`Pending → Delivered → InProgress → Done`).
  - When all entries on `commission<n>` reach `IndentureEntryStatus::Done`, `Ledger::mark_indenture_entry` transitions `commission<n>` `Active → WindingDown` (`CommissionEvent::AllEntriesDone`) and wakes the Mayor with `WakeReason::AllEntriesDone` (`WakeEvent::from_all_entries_done`).
- **CLI (`hamlet commission show <slug>`)**:
  - `CommissionDetail::render` now renders `  entries:    <uid> (<identity>): <status>, ...` when `indenture_entries` is non-empty.

### Verify

```sh
cargo test -p hamlet-core --lib commission         # 20: + indenture_ack_round_trip, entry_notice_subject_parser
cargo test -p hamlet-core --lib packet             # 8: + indenture_rejected & all_entries_done header rows
cargo test -p hamlet-daemon --test indenture       # 3 integration tests (no podman):
                                                   #   end-to-end creation -> Mayor decomposition decrees + INDENTURE.md
                                                   #     -> 2 provisioned fake agents -> indenture delivery -> progress/done
                                                   #     mails -> WindingDown + AllEntriesDone Mayor wake,
                                                   #   invalid INDENTURE.md self-healing retry loop,
                                                   #   bounded 3-attempt retry exhaustion -> EscalationFatal + Failed + notice
cargo test -p hamlet-cli --test commission_golden  # 3: + cli_commission_show_renders_indenture_entry_statuses
./scripts/ci-local.sh                              # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…07
```

### Open issues / carry-forward

- **Step 04 (daemon)**: git bundle mediation executor per ADR-0037 (`Reaction::GitBundle`, verification ladder, object graft into `guild/ledger/commission<n>/git/`, `refs/heads/agent/<uid>` update, `GitDiffReady` Mayor wake) and promotion executor per ADR-0038 (`ApprovePromotion` / `RejectPromotion` → vault copy + `DEED.md` dynamic alterations + `WindingDown → Closed`).

---

## Step 04 — runtime-engineer: agent work-branch loop (push mediation)

Status: **done**. Commit: `feat(daemon): git bundle push mediation (ADR-0012)`
(implements **ADR-0037** per the Step 01 ADR renumbering table). Consumes
**ADR-0037** (git push mediation), **ADR-0036** (commission roster & active
state), **ADR-0027** (`WakeReason::GitDiffReady` / `EventKind::GitDiffReady`),
**ADR-0011** (`0700` subuid-owned parcel homes & `podman unshare` mediation),
and **ADR-0007** (`O_NOFOLLOW` + `create_new` writes).

### Done

- **`hamlet-core` addenda**:
  - `gitmed::{agent_topic_branch, agent_topic_branch_ref, is_own_agent_branch_ref}`:
    supports both `refs/heads/agent/<uid>` and optional single-segment
    `SafeName` topic branches `refs/heads/agent/<uid>/<topic>` (`ADR-0037 cl. 2`).
  - `tree::HamletTree::census_git_archive(commission, uid)`:
    `keep/census/archives/git/<commission>/<uid>` (`ADR-0037 cl. 6`).
- **Agent-side git seeding & workflow (`crates/hamlet-daemon/src/ledger.rs`, `census.rs`, `defaults/MAYOR.md`)**:
  - `agent_gitconfig(uid, identity)`: renders `/home/<uid>/.gitconfig` with
    `user.name = <identity> (parcel<uid>)`, `user.email = <uid>@hamlet.local`,
    `init.defaultBranch = agent/<uid>`, and `safe.directory = /var/git`.
  - `git_push_helper_script(uid)`: renders `/home/<uid>/.pixi/bin/hamlet-git-push`
    (`0755`), which verifies `HEAD` (or target branch) matches
    `refs/heads/agent/<uid>[/<topic>]`, writes a hidden temp bundle
    `.staging-$$.tmp` (`0600`), and atomically renames it to
    `/home/<uid>/git-spool/<name>.bundle`.
  - `seed_agent_git_home(runtime, identity, uid)`: called at parcel provisioning
    (`Provisioner::provision_steps` after `bootstrapper.bootstrap_home`) to
    create `/home/<uid>/git-spool/` (`0700`), `/home/<uid>/.gitconfig` (`0600`),
    and `/home/<uid>/.pixi/bin/hamlet-git-push` (`0755`) inside the container
    without touching `.profile` or `.hamlet/toolchain.json`.
  - Documented the work-branch bundle mediation flow in `defaults/MAYOR.md`,
    `agent_commission_brief_addendum`, `proprietor_indenture_section`, and
    `compose_indenture_mail`.
- **Daemon-side git bundle mediator (`crates/hamlet-daemon/src/ledger.rs`)**:
  - `Ledger::process_git_bundle(uid, bundle_name)` / `reconcile_git_spool(uid)` /
    `reconcile_git_spools()` / `spawn_git_spool_plane(ledger, dispatcher)`:
    1. Resolves `uid` to its active/draft commission via `charter.json` roster
       and verifies the candidate filename via `is_valid_bundle_name`.
    2. Inspects and copies `/home/<uid>/git-spool/<name>.bundle` with
       `O_NOFOLLOW` (falling back to `podman unshare` when the `0700` parcel
       home is subuid-owned on the host per `ADR-0011 cl. 4`), refusing
       symlinks, non-regular files, world/group-writable permissions, or
       mismatched file owner UID (`BundleRejection::ForeignIdentity`), and
       enforcing `size <= MAX_BUNDLE_BYTES` (`32 MiB`,
       `BundleRejection::Oversize`).
    3. Unpacks the bundle into an isolated temporary quarantine bare repo linked
       via `objects/info/alternates` to `guild/ledger/commission<n>/git/objects`
       with `core.hooksPath=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` — **rejected
       bundles never write a single loose object or pack into
       `commission<n>/git/`**.
    4. Runs `git bundle list-heads` and `git bundle verify` (`BundleRejection::VerifyFailed`),
       enforces exactly one advertised head matching `is_own_agent_branch_ref(uid, ..)`
       (`BundleRejection::NotOwnBranch`), enforces fast-forward ancestry from
        the current agent branch tip or `refs/heads/main`
       (`BundleRejection::NonFastForward`), and walks all introduced commits
       verifying `%ae` and `%ce` both equal `<uid>@hamlet.local`
       (`BundleRejection::ForeignIdentity`).
    5. **On accept**: fetches verified objects into `commission<n>/git/`,
       updates `<branch_ref>` via `git update-ref <ref> <head> <old>`,
       re-normalizes bare repo permissions (`0755`/`0644`) so `:ro` `/var/git`
       stays readable inside containers, archives the bundle to
       `keep/census/archives/git/<n>/<uid>/<name>.bundle`, removes `<name>.bundle`
       from `/home/<uid>/git-spool/`, writes `<name>.bundle.ack` (`accepted`),
       records a `git_bundle_accepted` sled trace, and fires
       `CommissionWake::git_diff_ready` carrying the unified diff (`<= 64 KiB`)
       or deterministic `git diff --stat` summary (`> 64 KiB`).
    6. **On reject**: leaves `commission<n>/git/` untouched (`git fsck --full`
       clean), renames `<name>.bundle` to `<name>.bundle.<token>`
       (`oversize`/`verify`/`ref`/`identity`/`nonff`), writes `<name>.bundle.ack`
       (`rejected`), delivers a daemon-authored (`From: system`) refusal mail to
       `/var/mail/<uid>/`, and records a `git_bundle_rejected` sled trace.

### Verify

```sh
cargo test -p hamlet-core --lib gitmed                        # 5: + topic branch & own-branch ref validation
cargo test -p hamlet-core --lib tree                          # 9: + census_git_archive accessor
cargo test -p hamlet-daemon --test git_mediation              # 5 integration tests (no podman):
                                                              #   seeded + sequential FF pushes + topic branch + archive + ack + wake + sled,
                                                              #   unborn main first push + >64 KiB diff degradation to --stat summary,
                                                              #   hamlet-git-push helper script execution + foreign-branch refusal,
                                                              #   full ADR-0037 rejection matrix (oversize, corrupt, main, other-agent,
                                                              #     multi-ref, forged author, forged committer, wrong owner uid,
                                                              #     world-writable, symlink, non-FF) + git fsck --full + refusal mail + sled,
                                                              #   spawn_git_spool_plane startup recovery + live Reaction::GitBundle watcher
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon \
  --test git_mediation_guarded -- --ignored                   # real-podman container clone /var/git + hamlet-git-push + host reconciliation
./scripts/ci-local.sh                                         # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…07
```

---

## Step 05 — daemon-engineer: DEED dynamic-alterations → blueprint evolution (SPEC §9.4)

Status: **done**. Commit: `feat(daemon): DEED dynamic-alterations → blueprint evolution (SPEC §9.4)`.
Consumes **SPEC §5, §9.4, §10**, **ADR-0038** (DEED dynamic alterations grammar and promotion schema),
**ADR-0006** (crash-only atomicity), **ADR-0007** (symlink refusal), **CONVENTIONS §3 r4** (lineage-only timestamps),
and **CONVENTIONS §9** (dual-state immutable templates).

### Done

- **`hamlet-core` additions**:
  - `tree::HamletTree::guild_blueprint_version(&self, name: &SafeName, version: u32) -> PathBuf`: returns `guild/blueprints/<name>.v<version>.toml` for the append-only versioning layout.
  - `blueprint::Blueprint::render_toml(&self, provenance: Option<&str>) -> String`: canonical, deterministic TOML serialization preserving schema order (`[meta]`, `[env]`, `[[tools]]`), optional provenance header comments, and full round-trip fidelity.
  - `promote::{parse_deed_blueprint_ref, is_already_merged, alterations_digest}`:
    - `parse_deed_blueprint_ref`: extracts parent blueprint name from DEED header (`- blueprint: <name> v<version>...`).
    - `is_already_merged`: pure dedupe check comparing parsed alterations against blueprint toolset (same kind/name/version_spec).
    - `alterations_digest`: FNV-1a/64 content hash over canonical alteration tokens for tamper-evident provenance and dedupe tracking.
    - Robust baseline floor detection in `parse_deed_alterations`: skips `- baseline · toolchain bootstrap ...` and its sub-bullets (`- manifest:`, `- tool:`, `- image layer`, `- env keys:`, or indented lines) as the environment floor (SPEC §9.4).
- **`hamlet-daemon` promoter module (`crates/hamlet-daemon/src/promote.rs`)**:
  - `DeedPromoter`: manages per-parcel DEED parsing and blueprint promotion.
  - `ParsedDeedView`: maintains an incremental, checksummed parsed view of `### Dynamic Alterations` per parcel, updated in memory on `Reaction::DeedMutation` without rewriting agent history inside active parcels.
  - **Validation & Rejection Matrix**: validates alteration lines against blueprint `[tools]` grammar (`kind ∈ {pixi, nvm, cargo, apk}`). Injections, shell metacharacters, path separators, illegal line endings (CR bytes per ADR-0019 cl. 1), duplicate tools in a section, and duplicate sections are rejected with:
    - Line-numbered rejection report mailed to the agent (`From: system`, `Subject: commission:<n>:deed:rejected (<slug>)`).
    - Sled trace (`deed_alteration_rejected`).
    - Blueprint remains completely untouched.
  - **Evolution & Provenance**: valid alterations produce a new `Blueprint` via `apply_alterations` with bumped version and a top-of-file provenance header (`# Promoted from commission <n>, parcel <uid>, alterations digest <hex>, at <unix_ts>`).
  - **Append-Only Versioning Layout**:
    - Writes immutable version snapshot at `guild/blueprints/<name>.v<new_version>.toml` (`0644`).
    - Atomically updates active declaration at `guild/blueprints/<name>.toml` via tempfile + sync + atomic rename (`0644`).
    - Existing parcels retain their bootstrap manifests.
  - **Idempotence**: re-promoting an already-merged alteration set is a clean no-op (`PromotionOutcome::AlreadyMerged`) via content-hash and toolset dedupe.
  - **Symlink Refusal**: `O_NOFOLLOW` / `symlink_metadata` checks at both DEED and blueprint paths refuse symlinks (ADR-0007).
  - `spawn_deed_plane(promoter, dispatcher)`: background notify plane routing `Reaction::DeedMutation` to promoter cache.
- **Ledger Integration (`crates/hamlet-daemon/src/ledger.rs`)**:
  - `Ledger`: `with_promoter`, `with_promotion_trigger`, `promote_parcel_deed`, `promote_commission_deeds`.
  - Supports both `PromotionTrigger::EntryCompletion` (default) and `PromotionTrigger::CommissionWindDown`.
  - In `mark_indenture_entry`: automatic trigger runs upon agent entry completion (`IndentureEntryStatus::Done`) under `EntryCompletion`, or upon all entries done under `CommissionWindDown`.

### Decisions

- **Append-only file layout (`<name>.v<version>.toml` + `<name>.toml`)**: historical snapshots `<name>.v<version>.toml` are written immutably, and `<name>.toml` is atomically replaced to represent the current active version. Existing parcels retain their original bootstrap manifests; future parcels use the evolved blueprint.
- **Default trigger: `EntryCompletion`**: alterations from completed entries are promoted immediately upon reaching `Done` status, allowing subsequent workers to leverage evolved blueprints. `CommissionWindDown` is fully supported for workflows that defer blueprint promotion to commission close.
- **Provenance header as lineage data (CONVENTIONS §3 r4)**: Unix epoch timestamps are emitted only in the top-of-file provenance comment for auditability, never in semantic state or hash inputs.

### Verify

```sh
cargo test -p hamlet-core --lib promote              # 15: baseline skip, blueprint ref, already-merged, digest, grammar matrix
cargo test -p hamlet-core --lib blueprint            # 13: canonical TOML render, provenance header, round-trip
cargo test -p hamlet-daemon --test promote           # 9 integration tests (no podman):
                                                     #   golden evolution + version diffs + provenance headers,
                                                     #   rejection matrix (unknown kind, injections, shell metachars,
                                                     #     path separators, invalid versions, non-LF, duplicates, multi-section),
                                                     #   idempotence & content-hash dedupe,
                                                     #   atomic writes & symlink refusals (DEED and blueprint),
                                                     #   notify watcher Reaction::DeedMutation incremental view,
                                                     #   ledger triggers: EntryCompletion vs CommissionWindDown
cargo test -p hamlet-daemon --test ledger            # 8: roster management & commission creation
cargo test -p hamlet-daemon --test indenture         # 3: end-to-end indenture lifecycle & retry loops
./scripts/ci-local.sh                                # fmt, clippy -D warnings, workspace tests, check-deps, e2e-01…07
```

### Open issues / carry-forward

- None for Sprint 06. Ready for sprint completion.

---

## Step 06 — daemon-engineer: deliverable promotion loop + vault seam + commission close (SPEC §7, §9, §11.2)

Status: **done**. Commit: `feat(daemon,cli): vault promotion loop + commission close (SPEC §7, §9)`.
Consumes **SPEC §7** (validation loop), **SPEC §9.4**, **SPEC §11.2**, **SPEC §12.1**, **ADR-0006** (crash-only atomicity), **ADR-0007** (symlink refusal), **ADR-0026** (decrees), **ADR-0036** (commissions), **ADR-0038** (deliverable vault schema), and **CONVENTIONS §9** (file-as-truth).

### Done

- **`hamlet-core` additions**:
  - `tree::HamletTree::keep_vault(&self) -> PathBuf`: returns `keep/vault/`.
  - `tree::HamletTree::keep_vault_commission(&self, commission_slug: &SafeName) -> PathBuf`: returns `keep/vault/<commission-slug>/`.
  - `tree::HamletTree::vault_commission_artifact(&self, commission_slug: &SafeName, artifact: &SafeName) -> PathBuf`: returns `keep/vault/<commission-slug>/<artifact>`.
  - `tree::HamletTree::keep_archives_commission(&self, commission: u64) -> PathBuf`: returns `keep/archives/<commission>/`.
  - `commission::parse_deliverable_notice_subject(subject: &str) -> Option<(u64, Option<String>)>`: extracts commission ID and optional deliverable subpath from `commission:<n>:deliverable` or `commission:<n>:submit`.
  - `packet::WakeReason::DeliverableSubmitted`: token `"deliverable_submitted"`.
  - `packet::EventKind::DeliverableSubmitted { commission: u64, uid: u32, path: Option<String> }`: context packet event kind and formatting.
  - `commission::CommissionRecord::apply_event`: resets `deliverable_in_review = false` when `PromotionApproved` or `PromotionRejected` is applied, closing the review lifecycle cleanly.
  - `charter::CharterError`: implements `From<CommissionError>` for clean error conversion across charter and ledger boundaries.
- **`hamlet-daemon` vault promotion & commission close (`crates/hamlet-daemon/src/vault.rs` and `ledger.rs`)**:
  - `VaultPromotion`: implements `Promotion` seam (`crates/hamlet-daemon/src/decrees.rs`) for `ApprovePromotion` and `RejectPromotion` decrees.
  - **`ApprovePromotion`**:
    - Verifies freight file exists and is a regular file; enforces `O_NOFOLLOW` / `symlink_metadata` (ADR-0007) and root escape checks.
    - Atomically copies finished product into `keep/vault/<commission-slug>/<artifact>` (`0644`).
    - Emits `.sha256` checksum sidecar (`keep/vault/<commission-slug>/<artifact>.sha256`).
    - Advances commission state to `Closed` via `PromotionApproved`.
    - Marks submitting parcel in stasis (`lease.stasis = true`).
    - Initiates graceful session stop via `Supervisor::stop_session`.
    - Exports telemetry summary to `keep/archives/` via `Telemetry::export_commission_summary` and logs `vault_promoted` trace.
  - **`RejectPromotion`**:
    - Increments rejection count for the deliverable freight path.
    - Delivers rejection report to the agent via mail (`From: system`, `Subject: commission:<n>:deliverable:rejected (<slug>)`).
    - Bounded retry loop: up to `max_rejections` (default 3), resets `deliverable_in_review = false` allowing the agent to rework and re-submit.
    - On bound exhaustion: files an `EscalationNotice` in `keep/census/manor/escalations/escalation-commission<id>.json` and transitions commission to `Failed` (`PromotionRejected`).
  - **Mail Plane Integration (`mailroute.rs`)**:
    - Routes `Subject: commission:<n>:deliverable` addressed to the Mayor (`To: 1000`) through `Ledger::handle_deliverable_notice`.
    - Automatically transitions commission to `deliverable_in_review = true` and wakes the Mayor with `WakeEvent::from_deliverable_submitted`.
  - **Operator Commission Close (`ledger.rs`)**:
    - `Ledger::close_commission(slug: &str) -> Result<CommissionCloseReport, LedgerError>`: transitions active or winding down commission to `Closed`, parks all rostered parcels into stasis, exports final telemetry archive, and prunes active sled keys.
- **CLI Commands (`hamlet-cli`)**:
  - `hamlet commission close <slug> [--dir <dir>]`: operator command closing commission and exporting archives.
  - `hamlet vault list [--commission <slug>] [--dir <dir>]`: lists promoted deliverables across all or specific commissions with size and sha256 checksums in standard padded table format.
  - `hamlet vault export <artifact> <dest> [--commission <slug>] [--dir <dir>]`: exports promoted deliverable from `keep/vault/` to `<dest>`, enforcing symlink refusal (ADR-0007) and disambiguation when multiple commissions have identical artifact names.
- **Tests & Verification**:
  - `crates/hamlet-daemon/tests/vault.rs`: 5 integration tests covering full submission -> wake -> approve promotion loop, rejection rework loop, bounded rejection escalation to failed, symlink refusal and traversal security, and operator commission close.
  - `crates/hamlet-cli/tests/vault_golden.rs`: 3 golden/integration tests for `hamlet vault list`, `hamlet vault export`, `hamlet commission close`, security refusals, and usage errors (exit code 2).
  - All workspace tests and `./scripts/ci-local.sh` pass cleanly.

### Decisions

- **Symlink checks prior to canonicalization**: `symlink_metadata` is checked before `canonicalize()` to prevent following symlinks that resolve outside or inside the village root (ADR-0007).
- **SafeName single-dot constraint**: deliverable artifacts adhere to `SafeName` shape restrictions (≤1 extension dot), e.g. `bundle.tar` rather than `bundle.tar.gz`.
- **Rework state retention**: during deliverable rework following a non-fatal rejection, commission state remains in `WindingDown` while clearing `deliverable_in_review = false`, permitting agent rework and re-submission without re-activating decomposition.

### Verify

```sh
cargo test -p hamlet-core --lib commission
cargo test -p hamlet-core --lib packet
cargo test -p hamlet-daemon --test vault
cargo test -p hamlet-cli --test vault_golden
cargo test --workspace
./scripts/ci-local.sh
```



## Step 07 — qa-engineer: adversarial lifecycle suite + e2e script (SPEC §7, §9, §11.2; ADR-0036, ADR-0037, ADR-0038)

Status: **done, gate FAIL on 4 HIGH + 1 MEDIUM findings**. Commit:
`test(daemon,scripts): adversarial commission lifecycle suite + e2e script + findings (SPEC §7, §9)`.
Strict role boundary maintained: **no product code was modified**. Only test suites, e2e harnesses, and documentation were written.

### Committed Artifacts

- **`crates/hamlet-daemon/tests/adversarial_lifecycle.rs`** — 22 attack and invariant tests, FakeAgent mayor and workers, no podman required.
  - Covers all 5 mandated Sprint 06 surface areas:
    1. **Git mediation**: corrupt/truncated bundles, foreign-namespace branches (`refs/heads/main`, `refs/heads/foreign`), forged author identity (author != slot UID, author != committer), oversized bundles (>MAX_BUNDLE_BYTES), hook smuggling in bundle objects (`hooks/pre-receive`, `.git/hooks`), non-fast-forward push attempts — all rejected with clean fsck verification (`git fsck --full`) on the bare repository after every attack attempt.
    2. **DEED promotion**: injection lines (shell metachars `;`, `&&`, newlines `\n`, path separators `/`), unknown tool kinds rejected with typed errors and mail bounce, dup-set idempotence, kill -9 mid-promotion leaves canonical blueprint intact and parseable.
    3. **Vault loop**: nonexistent freight path rejected, artifact name traversal attempts (`../`, `/etc/passwd`) rejected by SafeName, double-approve replay refused on closed commission, reject-loop bound escalation fires and marks Failed with human escalation notice.
    4. **State machine & multi-tenancy**: total transition table enforcement (every illegal transition pair refused), roster mutations frozen in WindingDown/Closed/Failed, two commissions maintain strictly isolated rosters, bare git repositories, and state machines.
    5. **Restart resumability**: in-flight commission recovery with posted indenture, staged git bundle, and unpromoted deliverable resumes on restart without duplication; spool reconciliation idempotent.
    6. **QA findings reproduction**: live, deterministic repro tests for QA06-01 through QA06-05.
- **`scripts/e2e-commission.sh`** — deterministic, podman-optional end-to-end lifecycle script (executable, 4 tiers):
  - **T1**: CLI-level full commission lifecycle flow: `hamlet init` into temporary village → `hamlet commission new watchtower --brief ... --seed ...` → bare git repo verified clean with `git fsck --full` → Mayor decomposition (`INDENTURE.md`) → worker branch/bundle staged in git spool → freight deliverable produced in `courier/freight/` → vault promotion → `hamlet vault list` and `hamlet vault export` verified byte-identical → `hamlet commission close` → telemetry archive directory asserted.
  - **T2**: Adversarial lifecycle suite (22 tests, all green).
  - **T3**: Source-level ratchets for Sprint 06 invariants (bundle branch namespace pinned to `refs/heads/agent/<uid>`, author/committer emails pinned to `<uid>@hamlet.local`, bundle quarantine fetch before verification, state machine totality `IllegalTransition`, roster frozen outside Draft/Active, freight symlink refusal).
  - **T4**: Guarded podman commission check (podman-optional, skips cleanly when unprivileged containers are unavailable).

### What the Suite Proved GREEN

1. **Git Mediation Invariants (ADR-0037)**:
   - Corrupt, truncated, and random garbage bundles fail closed with `BundleRejection::CorruptBundle`; bare repository HEAD and objects remain pristine.
   - Pushes to branches other than `refs/heads/agent/<uid>` (e.g. `refs/heads/main`, `refs/heads/agent/1002`) fail closed with `BundleRejection::ForeignNamespace`.
   - Bundles containing commits with forged author or committer emails (author email != `<uid>@hamlet.local`) fail closed with `BundleRejection::ForgedIdentity`.
   - Bundles exceeding `MAX_BUNDLE_BYTES` (16 MiB) are rejected immediately at stat time with `BundleRejection::OversizeBundle`.
   - Hook smuggling attacks (planting executable scripts into `hooks/` or `.git/hooks/`) are rejected and cannot infect the bare repository.
   - Non-fast-forward bundle pushes are rejected with `BundleRejection::NonFastForward`.
   - Bare repository history is verified clean with `git fsck --full` after every single attack attempt.
2. **DEED Promotion Invariants (ADR-0009, SPEC §9.4)**:
   - Dynamic alteration lines containing shell metacharacters (`rm -rf /`, `&&`, `;`, `|`), newlines, or path traversal are rejected with typed errors, recorded in telemetry, and bounced back to the authoring agent via system mail.
   - Unknown tool kinds (e.g. `npm express = 4.18.2`) are rejected with typed errors and bounced back via system mail.
   - Repeated promotions of identical alteration sets are strictly idempotent (`PromotionOutcome::AlreadyMerged`), never bumping blueprint versions redundantly.
   - Simulating `kill -9` / power failure during atomic promotion leaves the canonical active blueprint valid, parseable, and intact at the previous version with zero torn files.
3. **Vault Loop Invariants (ADR-0038, SPEC §7)**:
   - Approving a deliverable with a nonexistent freight path is rejected with `RejectReason::InvalidTarget`.
   - Deliverable artifact names with directory traversal (`../`, `/etc/passwd`, `subdir/name`) are rejected at the SafeName boundary.
   - Double-approve replay on an already closed commission is refused with typed execution failure.
   - Rejection loop bound exhaustion (`count >= max_rejections`) transitions commission to `CommissionState::Failed`, delivers final rejection mail, and files an escalation notice in `keep/census/manor/escalations/escalation-commission<id>.json`.
4. **State Machine & Multi-Tenancy Invariants (ADR-0036, SPEC §9)**:
   - Totality: Every undefined `(state, event)` transition is refused with typed `IllegalTransition`. From `Closed`, all transitions are refused.
   - Roster freeze: `assign_uid` and `release_uid` are refused with `CommissionError::RosterFrozen` in `WindingDown`, `Closed`, and `Failed`.
   - Multi-tenancy: Two concurrent commissions maintain strictly disjoint rosters, separate bare git repositories, distinct ledger directories, and independent lifecycles.
5. **Restart Resumability Invariants (SPEC §9, §10)**:
   - When the daemon restarts mid-flight with a posted `INDENTURE.md`, a pending git bundle in spool, and an unpromoted deliverable: `reconcile_indentures` activates the commission and provisions workers, `reconcile_git_spools` ingests the bundle and writes `.bundle.ack`, and subsequent reconciliation passes are clean no-ops with zero duplicated events.

---

### Findings

#### QA06-01 — HIGH — DEED Non-Append Wholesale Mutation Accepted Without Rejection

- **Location**: `crates/hamlet-daemon/src/promote.rs:233-326` (`inspect_deed` and `promote_parcel`)
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_lifecycle qa06_01`
- **Expected**: SPEC §9.4 ("DEED blocks are append-only. Mutation of existing entries or deleting baseline is a protocol violation; daemon detects non-append mutation → reject + trace") and Deliverable 1 requirement.
- **Actual**: `DeedPromoter::inspect_deed` and `promote_parcel` overwrite the cached view unconditionally on read:
  ```rust
  let view = self.inspect_deed(uid)?;
  // ...
  // overwrites cached_views without verifying that previously recorded alterations
  // and the immutable baseline entry are preserved:
  map.insert(uid, view.clone());
  ```
  If a rogue agent rewrites `DEED.md` wholesale (deleting the baseline line, deleting past recorded alterations, substituting entirely different tools), the daemon computes alterations only against the blueprint's current tools rather than verifying append-only history. The promotion succeeds as `PromotionOutcome::Promoted`, allowing rogue agents to tamper with environment provenance and inject altered history into the village blueprint.
- **Citations**: SPEC §9.4, §10; ADR-0009 cl. 3; Deliverable 1 prompt.

#### QA06-02 — HIGH — Vault Freight Path Traversal Escapes Freight Directory Into Village

- **Location**: `crates/hamlet-daemon/src/vault.rs:149-160` (`resolve_freight_file`)
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_lifecycle qa06_02`
- **Expected**: SPEC §4, §9.4, and ADR-0038 require deliverables to be strictly confined to `courier/freight/`. Any path escaping `courier/freight/` (via intermediate symlink or relative path) must be rejected with `DecreeRefusal::invalid_target`.
- **Actual**: `resolve_freight_file` validates confinement against `canon_root` (the village root `.hamlet`), instead of `self.tree.courier_freight()`:
  ```rust
  // Escapes root check
  let canon_root = match self.tree.root().canonicalize() {
      Ok(r) => r,
      Err(_) => self.tree.root().to_path_buf(),
  };
  if let Ok(canon_target) = target.canonicalize() {
      if !canon_target.starts_with(&canon_root) {
          return Err(DecreeRefusal::invalid_target(format!(
              "freight path {freight_path} escapes village root"
          )));
      }
  }
  ```
  While `symlink_metadata` checks if the *target leaf* is a symlink, intermediate directory symlinks inside `courier/freight/` (e.g. `courier/freight/link -> ../../keep/census/manor/home`) are followed by path traversal. Because the canonical target is within the village root `.hamlet`, the confinement check passes, allowing an agent to copy arbitrary village files (e.g. `charter.json`, `keep/census/manor/MAYOR.md`, other agents' private parcel homes) into `keep/vault/` as promoted deliverables.
- **Citations**: SPEC §4, §9.4; ADR-0007, ADR-0038 cl. 1.

#### QA06-03 — HIGH — Vault Atomic Write Follows Planted Symlink Tempfiles

- **Location**: `crates/hamlet-daemon/src/vault.rs:441` (`write_atomic_file`) and `crates/hamlet-cli/src/vault.rs:721` (`write_atomic_export`)
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_lifecycle qa06_03`
- **Expected**: SPEC §1 (file-as-truth), §2 (typed paths), and ADR-0007 require atomic write tempfiles to be created with `create_new(true)` (`O_CREAT | O_EXCL`) or opened `O_NOFOLLOW` so planted symlinks cannot redirect daemon writes to clobber other files (exact duplicate of QA05-04).
- **Actual**: Both `write_atomic_file` in the daemon and `write_atomic_export` in the CLI use predictable temp file names `.{file_name}.tmp-{pid}` with `OpenOptions::new().write(true).create(true).truncate(true)`, which follows symlinks:
  ```rust
  let temp_path = dir.join(format!(".{}.tmp-{}", file_name, std::process::id()));
  let mut file = fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .open(&temp_path)?;
  ```
  An adversary planting a symlink at `keep/vault/<commission>/.<artifact>.tmp-<pid>` pointing to a victim file (e.g. `charter.json`) causes the daemon to follow the symlink and truncate/overwrite the victim file with the vault artifact payload.
- **Citations**: SPEC §1, §2; ADR-0007; QA05-04 precedent.

#### QA06-04 — HIGH — Vault Promotion Cross-Contaminates Between Commissions

- **Location**: `crates/hamlet-daemon/src/vault.rs:170-178` (`identify_commission`)
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_lifecycle qa06_04`
- **Expected**: SPEC §4 (isolation invariants), §9.4 (deliverable promotion), and ADR-0036 require deliverable promotion to strictly associate a freight artifact with the specific commission that authored it, without cross-contamination.
- **Actual**: `VaultPromotion::identify_commission` checks whether *any* commission has `deliverable_in_review == true` in ascending key order before inspecting freight path components:
  ```rust
  // 1. Check if any commission has deliverable_in_review == true
  if let Some((&id, comm)) = charter
      .commissions
      .iter()
      .find(|(_, c)| c.deliverable_in_review)
  {
      let submitting_uid = comm.roster.iter().next().copied();
      return Ok((id, comm.slug.clone(), submitting_uid));
  }
  ```
  If Commission 1 is in review and Commission 2's worker produces a deliverable in `courier/freight/beta-worker/product.tar`, approving Commission 2's deliverable selects Commission 1 instead! Commission 1 is falsely closed, Commission 1's vault receives Commission 2's artifact, Commission 1's worker is placed in stasis, while Commission 2 remains unclosed. Furthermore, step 3 of `identify_commission` arbitrarily falls back to the first active/winding-down commission in key order.
- **Citations**: SPEC §4, §9.4; ADR-0036 cl. 2, ADR-0038 cl. 1.

#### QA06-05 — MEDIUM — Vault Copy Checksum Unverified and Sidecar Write Failures Silently Ignored

- **Location**: `crates/hamlet-daemon/src/vault.rs:270-281` (`approve`)
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_lifecycle qa06_05`
- **Expected**: SPEC §7, §9.4, and ADR-0038 require that the copied vault artifact is verified on disk against the computed SHA-256 digest before finalizing promotion, and that `.sha256` sidecar generation is mandatory.
- **Actual**: `VaultPromotion::approve` calculates the digest while reading source bytes, writes the target file, and then ignores the outcome of writing the checksum sidecar:
  ```rust
  let sidecar_content = format!("{checksum}  {artifact_name}\n");
  let _ = write_atomic_file(&sidecar_path, sidecar_content.as_bytes(), MODE_FILE);
  ```
  If writing the `.sha256` sidecar fails (e.g. disk quota, permission, directory conflict), the failure is silently discarded (`let _ = ...`), leaving the vault artifact without an integrity sidecar. Furthermore, the bytes written to disk are never re-hashed to verify that the copy was uncorrupted before advancing charter state to `Closed`.
- **Citations**: SPEC §7, §9.4; ADR-0038 cl. 1.

---

### Gate Verdict

`GATE 06/07: FAIL` (superseded by step 07b: all 4 HIGH + 1 MEDIUM findings resolved with inverted regression tests)

Reason: 4 HIGH findings (QA06-01 wholesale DEED mutation acceptance, QA06-02 vault freight path traversal escape, QA06-03 planted symlink tempfile clobber, QA06-04 cross-commission vault deliverable misattribution) and 1 MEDIUM finding (QA06-05 unverified vault copy digest and silent sidecar write failure). In accordance with QA doctrine, no product code was altered; findings are documented above with precise reproductions for the daemon engineer's remediation pass.

---

## Step 07b — daemon-engineer: QA06-01/02/03/04/05 lifecycle remediation (SPEC §7, §9, §11.2; ADR-0007, ADR-0036, ADR-0038)

Status: **done**. All 4 HIGH items + 1 MEDIUM finding resolved in product code with inverted regression tests.

### Done

- **QA06-01 (HIGH) — DEED Non-Append Wholesale Mutation Rejection (SPEC §9.4)**:
  - Added `DeedError::NonAppendMutation { detail: String }` to `hamlet_core::promote`.
  - Updated `DeedPromoter` (`crates/hamlet-daemon/src/promote.rs`) to track `recorded_alterations` and `has_baseline` in `ParsedDeedView`.
  - In `inspect_deed`: compares against the previous cached view:
    1. If baseline was present previously (`baseline_required`) and is now missing: returns `DeedError::NonAppendMutation` ("baseline entry was deleted").
    2. If previously recorded alterations are not a prefix of current alterations (`!new_alts.starts_with(&recorded_alterations)`): returns `DeedError::NonAppendMutation` ("previously recorded alterations were modified or deleted").
  - On `DeedError::NonAppendMutation`, `promote_parcel` rejects promotion (`PromotionOutcome::Rejected`), sends a line-numbered rejection report mail to the agent, traces `deed_alteration_rejected` in telemetry, and leaves the parent blueprint pristine.

- **QA06-02 (HIGH) — Vault Freight Path Traversal Confinement (SPEC §4, §9.4; ADR-0007, ADR-0038)**:
  - In `VaultPromotion::resolve_freight_file` (`crates/hamlet-daemon/src/vault.rs`):
    1. Strictly confines target paths under `self.tree.courier_freight()`.
    2. Iterates component-by-component from `courier_freight` down to the target leaf, asserting `!meta.file_type().is_symlink()` at every intermediate segment (`DecreeRefusal::invalid_target`).
    3. Resolves canonical path of both freight directory and target, asserting `canon_target.starts_with(&canon_freight)`. Intermediate symlinks escaping freight into the village (e.g. manor home or charter) are refused with `DecreeRefusal::invalid_target`.

- **QA06-03 (HIGH) — Vault Atomic Write Follows Planted Symlinks (ADR-0007, SPEC §1, §2)**:
  - In `crates/hamlet-daemon/src/vault.rs::write_atomic_file` and `crates/hamlet-cli/src/vault.rs::write_atomic_export`:
    - Replaced `create(true).truncate(true)` with ADR-0007 create-new discipline: `OpenOptions::new().write(true).create_new(true)`.
    - Added bounded collision sequence walk (`TEMP_ATTEMPTS = 256`). Planted symlink tempfiles are skipped without opening, truncating, or following them.

- **QA06-04 (HIGH) — Cross-Commission Deliverable Misattribution (SPEC §4, §9.4; ADR-0036, ADR-0038)**:
  - In `VaultPromotion::identify_commission` (`crates/hamlet-daemon/src/vault.rs`):
    - Path components of `freight_path` are evaluated FIRST before considering in-review commissions:
      1. Direct commission slug match (`comm.slug == part`).
      2. Slot identity match (`slot.identity == part`) mapped to its rostered commission.
      3. Slot UID match (`uid == part`) mapped to its rostered commission.
      4. Commission ID match (`commission<n>` or `<n>`).
    - Only if path components do not identify a commission/slot: checks commissions with `deliverable_in_review == true`. If ambiguous (multiple in review), refuses with `DecreeRefusal::invalid_target` rather than misattributing to the first commission in key order.

- **QA06-05 (MEDIUM) — Vault Checksum Verification & Mandatory Sidecar Write (SPEC §7, §9.4; ADR-0038)**:
  - In `VaultPromotion::approve` (`crates/hamlet-daemon/src/vault.rs`):
    - Re-reads written vault artifact from disk and asserts `content_digest(&written_bytes) == checksum`.
    - Enforces `.sha256` sidecar creation: errors writing sidecars are propagated (`DecreeRefusal::execution_failed`) rather than silently swallowed.

### Verify

- `cargo test -p hamlet-daemon --test adversarial_lifecycle` (22/22 green, QA06-01..05 inverted into regression tests).
- `cargo test -p hamlet-daemon --test vault` (5/5 green).
- `cargo test -p hamlet-daemon --test promote` (9/9 green).
- `cargo test --workspace` (all unit/integration tests green).
- `./scripts/e2e-commission.sh` (T1 CLI flow, T2 adversarial suite, T3 source ratchets: PASS).
- `./scripts/ci-local.sh` (fmt, clippy, check-deps, all workspace and e2e suites: PASS).


## Step 07c — qa-engineer: adversarial suite re-run & gate verdict (GATE 06/07: PASS)

Status: **done, gate PASS on all 4 HIGH + 1 MEDIUM remediations verified**. Commit:
`test(daemon,scripts): verify sprint 06 remediation pass, all adversarial suites green (SPEC §7, §9)`.
Strict role boundary maintained: **no product code was modified by QA**.

### Remediation Verification Matrix

| Finding | Severity | Surface | Remediation Verified | Regress Test | Status |
|---|---|---|---|---|---|
| **QA06-01** | **HIGH** | DEED non-append wholesale mutation | `DeedPromoter` detects deleted baseline and non-prefix alterations; returns `DeedError::NonAppendMutation`, bounces agent mail, and traces rejection. | `adversarial_lifecycle::qa06_01` | **PASS** |
| **QA06-02** | **HIGH** | Vault freight path traversal escape | `resolve_freight_file` confines targets strictly to `courier/freight/`, asserts no intermediate symlinks across all path components, and checks `canon_target.starts_with(&canon_freight)`. | `adversarial_lifecycle::qa06_02` | **PASS** |
| **QA06-03** | **HIGH** | Vault atomic write follows planted symlinks | `write_atomic_file` & `write_atomic_export` use `create_new(true)` (`O_CREAT\|O_EXCL`) with bounded collision walk (`TEMP_ATTEMPTS = 256`); planted symlink tempfiles are skipped without following or clobbering victim files. | `adversarial_lifecycle::qa06_03` | **PASS** |
| **QA06-04** | **HIGH** | Cross-commission vault deliverable misattribution | `identify_commission` evaluates path components (commission slug, slot identity, slot UID) before considering in-review commissions; multiple in-review fallbacks fail closed with ambiguous target refusal. | `adversarial_lifecycle::qa06_04` | **PASS** |
| **QA06-05** | **MEDIUM** | Vault copy checksum unverified & silent sidecar failure | `approve()` re-reads written vault artifact on disk and verifies `content_digest == checksum`; `.sha256` sidecar generation is mandatory and propagates write errors. | `adversarial_lifecycle::qa06_05` | **PASS** |

### Suite Results

- `cargo fmt --all --check`: **PASS**
- `cargo clippy --workspace --all-targets -- -D warnings`: **PASS** (0 warnings)
- `cargo test -p hamlet-daemon --test adversarial_lifecycle`: **PASS** (22/22 green)
- `cargo test -p hamlet-daemon --test vault`: **PASS** (5/5 green)
- `cargo test -p hamlet-daemon --test promote`: **PASS** (9/9 green)
- `cargo test --workspace`: **PASS**
- `./scripts/e2e-commission.sh`: **PASS** (T1 CLI flow, T2 adversarial suite, T3 source ratchets)
- `./scripts/ci-local.sh`: **PASS** (all 5 battery gates and 9 e2e scripts)

### Gate Verdict

`GATE 06/07: PASS`

All 4 HIGH findings (QA06-01, QA06-02, QA06-03, QA06-04) and the 1 MEDIUM finding (QA06-05) have been verified fixed in product code with inverted regression assertions in `crates/hamlet-daemon/tests/adversarial_lifecycle.rs`. The full lifecycle test battery and `scripts/e2e-commission.sh` pass cleanly across all tiers.

---

## Step 08 — ci-agent: Sprint 06 verification battery & gate verdict (GATE 06/08: PASS)

Status: **done, gate PASS**. Precondition `GATE 06/07: PASS` verified. Strict role boundary maintained: **no product code was edited**.

### Verification Battery Results

| Check / Gate | Target / Scope | Result | Details |
|---|---|---|---|
| `cargo fmt --all --check` | Workspace-wide code formatting | **PASS** | Formatted cleanly across all crates |
| `cargo clippy --workspace --all-targets -- -D warnings` | Workspace-wide static analysis | **PASS** | 0 warnings across all targets |
| `cargo test --workspace` | Full workspace unit & integration suites | **PASS** | All unit and integration suites green (100% pass) |
| `scripts/check-deps.sh` | Dependency graph & allowlist | **PASS** | All dependencies within locked allowlist |
| Guarded Podman Suite (`scripts/guarded-entry.sh`) | 7 guarded podman suites (`HAMLET_PODMAN_TESTS=1`) | **PASS** | 14/14 tests green across `podman_guarded`, `bootstrap_guarded`, `provision_guarded`, `escape_guarded`, `freight_guarded`, `supervise_guarded`, `manor_guarded` |
| `scripts/e2e-01-safename-compile-fail.sh` | SafeName compile-time failure | **PASS** | T1 compile error asserted |
| `scripts/e2e-01-topology-state.sh` | Topology & state invariant e2e | **PASS** | T1-T3 topology checks green |
| `scripts/e2e-02-isolation.sh` | Subuid isolation e2e | **PASS** | T1-T3 isolation checks green |
| `scripts/e2e-03-adversarial.sh` | Adversarial boundary harness | **PASS** | T1-T2 boundary tests green |
| `scripts/e2e-04-comms.sh` | Inter-agent communications e2e | **PASS** | T1-T3 comms checks green |
| `scripts/e2e-05-mayor-adversarial.sh` | Mayor adversarial e2e harness | **PASS** | T1 suite + T2 ratchets + T3 guarded smoke green |
| `scripts/e2e-06-guard-mutations.sh` | Guard mutation testing e2e | **PASS** | All mutation tests killed |
| `scripts/e2e-07-ci-abort.sh` | CI abort & verdict backstop e2e | **PASS** | T1-T3 abort paths and lock checks green |
| `scripts/e2e-commission.sh` | Sprint 06 Commission Lifecycle e2e | **PASS** | T1 CLI flow, T2 adversarial suite (22 tests), T3 source ratchets, T4 guarded check green |
| `scripts/ci-local.sh` | Local full battery execution | **PASS** | 100% clean exit (all battery gates + all e2e scripts) |

### ADR-0012 Implementation Verification

ADR-0012 (derived parcel images & pinned toolchains) and associated Sprint 06 decisions (ADR-0036 state machine, ADR-0037 git push mediation, ADR-0038 DEED alterations promotion) were verified against implementation:
- **ADR-0012 cl. 1–2 (Derived Images & Containerfile Generation)**: Parcel images are derived from parent `[meta].base_image` via deterministic tag (`v<bp>-f<floor>-c<fnv1a64>`); verified in `bootstrap.rs`, `podman.rs`, and guarded suites.
- **ADR-0012 cl. 3–4 (Pinned Toolchains & Environment Floor)**: Toolchain versions are strictly pinned via `toolchain-pins.toml`; no unpinned tools or floating versions are installed.
- **ADR-0012 cl. 5 / ADR-0038 (Baseline Floor vs Agent Alterations)**: `promote.rs` reads the baseline entry (`- baseline · ...`) as an immutable environment floor, not an agent alteration; deleted baseline or modified past alterations trigger `DeedError::NonAppendMutation` and mail rejection, satisfying SPEC §9.4 and ADR-0012 consequences.
- **ADR-0036 / ADR-0037**: Commission state machine totality (`IllegalTransition`), roster freezing, git bundle branch isolation (`refs/heads/agent/<uid>`), and author identity checks (`<uid>@hamlet.local`) verified across unit, integration, and e2e ratchets.

### Gate Verdict

`GATE 06/08: PASS`

