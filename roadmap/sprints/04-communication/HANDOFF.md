# HANDOFF — Sprint 04 Communication Planes

## Step 01 — architect: mail format, dispatch schemas, watcher vocabulary

Status: **done**. Commit: `feat(core): mail format, dispatch schemas,
watcher vocabulary (ADR-0019, ADR-0020)`.

### Done

- **`hamlet-core::mail`** (SPEC §8.1; ADR-0019 cl. 1–4) — plain-text mail
  with the fixed four-header preamble (`From:` uid|SafeName, `To:` slot uid,
  `Subject:` 1–128, `Date:` opaque lineage token ≤64), blank line, UTF-8
  body (non-empty, ≤64 KiB). LF-only: **any CR is a parse rejection**
  (header-injection defense). `Mail::parse`/`render` round-trip byte-exact.
  Spool naming `{seq:012}-{writer_uid}-{nonce}.mail` (`spool_file_name` /
  `parse_spool_file_name`) so per-writer FIFO = lexicographic scan order;
  `is_incoming` gates non-hidden `*.mail`. Typed `BounceReason` +
  `bounce_subject` (bounded, single-line). `is_slot_uid` reused from slots.
- **`hamlet-core::dispatch`** (SPEC §8.4; ADR-0020) — `OutboxRequest`
  (method/https-url/`headers_env` by-key-name/body/correlation, canonical
  alphabetical-key JSON, ≤1 MiB, `deny_unknown_fields`) and `InboxResponse`
  (`kind`-tagged `http`/`error`, headers lowercased+redacted+capped, body
  ≤64 KiB truncated). `UrlAllowlist` (exact-authority + path-prefix,
  https-only, empty=deny-all), `validate_url` (charset, no credentials),
  `RetryPolicy` (**deterministic jitter-free** bounded-exponential
  `min(initial·2^(n−2), max)`, `is_retryable_status` = 429|5xx),
  correlation↔`<corr>.json` file-name helpers. **The schema cannot express
  an inline secret** — `headers_env` maps header→`.env` key name only.
- **`hamlet-core::watch`** (ADR-0019 cl. 5–6) — typed `FsEvent`/`FsAction`
  → `Reaction` `classify` table covering mail spool additions, DEED.md
  mutations (Sprint 06), decree regions (`manor/home/**`,
  `commission<n>/INDENTURE.md`; Sprint 05), dispatch outbox additions,
  freight dir creations; hidden/absolute/traversal paths → `Ignored`.
  `EventBatch` (BTreeMap) merges per-path (Created⊃Modified, Created+Removed
  cancels, Removed+Created re-arms) and `drain`s in sorted-path order for
  deterministic reaction sequencing. `QUIESCE_MS = 250`.
- **`hamlet-core::config` v5** — additive `DispatchConfig` keys
  (`url_allowlist`, `max_attempts`, `backoff_initial_ms`, `backoff_max_ms`),
  validated against ADR-0020 ranges; `retry_policy()` projects a typed
  `RetryPolicy`. v4 documents still parse (serde defaults). Goldens updated.
- **`hamlet-core::tree`** — new accessors `courier_mail_read` (hidden
  `.read/` ack dir), `courier_dispatch_dead` (`dead/` dead-letter, now a
  static `ensure`d dir); golden path vectors added.
- **`hamlet-core::session`** — `CorrelationId::for_dispatch` (`d-<seq>-…`);
  canonical-form doc + `QueuedKind::Mail` doc pointed at ADR-0019/0020.

### Decisions (ADR refs)

- **ADR numbering:** the prompt cites "ADR-0009/0010", but those numbers are
  claimed (blueprint schema / podman policy). Per `docs/adr/README.md`
  renumbering rule, this work is **ADR-0019 (mail + watchers)** and
  **ADR-0020 (dispatch)**; README index + renumber table updated. Later
  sprint-04 prompts reading "ADR-0009/0010" must read **0019/0020**.
- **Ack marker location (file-as-truth, the prompt's open `[DEV]`):** acks
  live in the **tree**, as a rename into the recipient's hidden
  `courier/mail/<uid>/.read/` subdir — *not* in sled (telemetry-only,
  CONVENTIONS §2). **Enqueue-before-ack**: the daemon renames into `.read/`
  only after the Sprint 03 `QueueStore` has fsync-persisted the envelope to
  `courier/queues/<uid>.jsonl`, so a crash between the two re-delivers at
  most once from the spool and never silently drops. Restart scans spool top
  level only; `.read/` is never rescanned → **zero double-delivery**.
- **Dispatch correlation ≠ SafeName:** `<uid>.<inc>.json` carries two dots,
  so `Reaction::DispatchOutbox` transports the typed `CorrelationId` (parsed
  by `correlation_from_file_name`), not a `SafeName`. File names are
  charset-safe path segments built only via `file_name`, never from raw input.

### Open issues / carry-forward

- **Step 02 (watcher core + mail routing)** consumes ADR-0019: run `notify`
  over the tree, normalize events to village-relative `FsEvent`s, debounce
  via `QUIESCE_MS` + `EventBatch`, feed `classify` → handlers. Registration
  must be a clean extension point (DEED/decree/outbox handlers arrive in
  later sprints). Router maps `MailSpool` → parse/validate → charter
  cross-check → Sprint 03 queue enqueue → `.read/` ack; undeliverable →
  `BounceReason` bounce + sled trace. Restart mid-delivery must recover from
  ack state with no duplicates (test explicitly).
- **Step 03 (freight ACLs)** consumes `Reaction::FreightDir` (top-level dir
  creations only — nested paths are `Ignored` by design).
- **Step 04 (dispatch + axum)** consumes `Reaction::DispatchOutbox` and
  ADR-0020 end-to-end: allowlist check → reqwest POST → inbox response →
  retry/`RetryPolicy` → `dead/` dead-letter. Resolve `headers_env` from
  `.env` by key at send time; missing key = validation failure (cl. 4),
  never an empty-header send. Inbound webhook topic names pass the §4
  sanitizer.
- **Decree regions are provisional:** `classify` maps `manor/home/**` and
  `commission<n>/INDENTURE.md` to `DecreeWrite{path}` now; Sprint 05's
  encoding ADR may extend the table — `classify` is the single mapping point
  to amend (ADR-0019 cl. 5).

### Verify

```sh
cargo test -p hamlet-core mail        # format round-trip, injections, spool naming, bounces
cargo test -p hamlet-core dispatch    # golden outbox/inbox vectors, retry schedule, allowlist, url validation
cargo test -p hamlet-core watch       # reaction-table golden + batch merge/sorted drain
cargo test -p hamlet-core config      # v4→v5 additive migration, dispatch validation
cargo test -p hamlet-core tree        # courier_mail_read / courier_dispatch_dead goldens
./scripts/ci-local.sh                 # CI battery: PASS (fmt, clippy -D warnings, tests, check-deps, e2e)
```

Golden vectors committed inline (CONVENTIONS §9): `mail_render_golden`,
`outbox_render_golden`, `inbox_render_golden`, `retry_schedule_golden`,
`reaction_table_golden`, `default_config_render_golden`.

---

## Step 02 — daemon-engineer: watcher core + mail routing

Status: **done**. Commit: `feat(daemon): inotify watcher core + mail spool
routing (SPEC §8.1)`.

### Done

- **`hamlet-daemon::watch`** (SPEC §8; ADR-0019 cl. 5–6) — `notify`
  (inotify) recursive watcher over the whole `.hamlet/` tree. Single
  pipeline task: bounded raw channel (8192, `blocking_send` backpressure
  from the notify thread) → per-file quiesce (`QUIESCE_MS` = 250, folds via
  new pure `hamlet_core::watch::merge_actions` — same table `EventBatch`
  uses) → 50 ms sweep → `EventBatch` sorted-path drain → monotonic `seq` →
  `classify` → `Dispatcher` fan-out. Rename targets map to Created, sources
  to Removed; hidden components (writer temps, `.read/`, queue temps) are
  dropped before debouncing; access/other notify noise never enters.
- **`Dispatcher` — the handler registration extension point**: named
  bounded (4096) reaction streams; `register("name")` returns a receiver,
  fan-out in sorted-name order, `await`ed sends (a slow handler
  backpressures the watcher instead of growing the heap), dropped handlers
  are pruned. Mail registers now; DEED/decree/outbox/freight handlers
  (steps 03/04, Sprint 05/06) just `register` + filter variants — no
  changes to `watch`.
- **`hamlet-daemon::mailroute`** (SPEC §8.1; ADR-0019 cl. 2–4) — spool
  addition → `Mail::parse` → recipient/To cross-check → charter check
  (slot exists; `Crashed`/`Hibernating` refuse) → `Supervisor::enqueue`
  (Sprint 03 `QueueStore`, sequential stdin, bounded by
  `sessions.queue_depth`) → **enqueue-before-ack** rename into
  `courier/mail/<uid>/.read/` (collision suffix `.r<n>`). Undeliverable →
  original consumed + `From: system` bounce written to the sender's spool
  (typed `BounceReason`; body = reason + original headers, never the
  original body) + sled trace under the sender's agent key. Malformed mail
  bounces to the *file-name writer uid*; foreign names / named senders
  (`system`, patrons) die with a trace — bounce chains are bounded at
  depth 2 by construction.
- **Sending API** — `MailRouter::send(MailActor, to, subject, body, date)`:
  `MailActor` (Mayor/Agent(uid)/System/Patron(name)) **is** the `From:`
  identity — no sender parameter exists, so callers cannot claim an
  identity. Atomic write (hidden temp → fsync → rename → dir fsync),
  conforming `{seq:012}-<writer>-<nonce>.mail` names for slot actors
  (foreign `-system-`/`-<patron>-` names otherwise); seq from a new durable
  sled counter domain (`Telemetry::next_spool_seq`, DOMAIN 4) — monotonic
  across restarts, no wall-clock input (CONVENTIONS §3.4). Outgoing mail is
  re-parsed before writing: the daemon never emits a spool file it would
  bounce.
- **Restart recovery** — `recover_spools()` scans spool top levels only
  (`.read/` never rescanned) in sorted order; `spawn_mail_plane` runs it as
  the watcher-task **startup hook, strictly before the first dispatched
  event**, so recovery and event routing never race. Test
  `restart_mid_delivery_recovers_from_ack_state_without_duplicates`: route A
  → kill daemon objects → fresh Supervisor/QueueStore/Telemetry → recovery
  delivers B only; queue = [A, B] exactly once.
- **Storm resilience** — `storm_1k_spool_writes_all_delivered_fifo`: 1000
  atomic temp+rename writes across 10 recipients → all delivered, every
  recipient queue in exact seq order, all originals acked; bounded
  channels + single sequential handler task = bounded memory.

### Decisions

- **Reactions are hints, routing is a scan** (in-daemon, extends ADR-0019
  cl. 3): each `MailSpool` reaction triggers `route_recipient` — a *sorted
  scan of the whole recipient spool* in the single sequential handler task.
  Per-recipient FIFO no longer depends on event arrival/drain order, and
  lost inotify events self-heal on the next hint or restart. Idempotent:
  vanished files are cheap `AlreadyConsumed` no-ops.
- **Fresh-directory inotify race — closed from both ends.** Events inside a
  *just-created* dir can precede notify's watch attachment (proven by test
  failure, not theory). (1) `ensure_spool_dirs()` pre-creates a spool dir
  for every charter slot in the startup hook, so external writers never
  create dirs; (2) `send()` pushes a direct recipient hint into the handler
  channel, so in-daemon sends never depend on event timing at all.
- **Session-state bounce policy:** only `Crashed` and `Hibernating` refuse
  delivery (prompt's "crashed session"); `Starting/Running/Busy/Idle/
  Stopped` and no-record enqueue into the persistent queue (delivered at
  the next boundary/start — Sprint 03 semantics).
- **`hamlet-core::watch::merge_actions`** extracted (pub, pure, golden
  3×3 test) so the daemon's per-file debounce buffer and `EventBatch` fold
  events by the *same* committed table — one merge contract.
- **`Telemetry::next_spool_seq`** (additive, DOMAIN_SPOOL_SEQ = 4): durable
  per-writer spool seq counter; commission-0 key space, never pruned with
  commissions (name ordering must stay monotonic forever).

### Open issues / carry-forward

- **Step 03 (freight)** registers `Reaction::FreightDir` on the existing
  `Dispatcher` — no watcher changes needed. **Step 04 (dispatch)** likewise
  for `Reaction::DispatchOutbox`; note outbox/inbox/dead dirs are
  `ensure`d by the tree, so the fresh-dir race doesn't apply there.
- **Runtime slot allocation** (census/provisioning, Sprint 07 hibernation
  wake) should call `MailRouter::ensure_spool_dirs()` (or create the one
  spool dir) after allocating a slot, so mail to brand-new slots never
  races watch attachment.
- **Seq consumption:** `seq` increments for every drained event, including
  ones `classify` maps to `Ignored` — handler-visible seqs are monotonic
  but not gap-free (drain-order identity, not a delivery counter).
- **Delivery-test flake (pre-existing, Sprint 03):**
  `boundary_semantics_no_mid_task_writes` failed once under full-suite
  parallel load (missed one prompt), green in isolation and on CI re-run —
  timing-sensitive FakeAgent boundary wait; QA (step 05) may want to widen
  its poll window. Unrelated to this step's changes.

### Verify

```sh
cargo test -p hamlet-daemon --test watch      # 7: reactions, hidden noise, blip cancel, dir payloads, startup order, fan-out, stop
cargo test -p hamlet-daemon --test mailroute  # 16: deliver/ack/trace, FIFO scan, 5 bounce classes, send API, restart-no-duplicates, plane e2e, 1k storm
cargo test -p hamlet-core watch               # merge_actions golden table + reaction goldens
./scripts/ci-local.sh                         # CI battery: PASS (fmt, clippy -D warnings, workspace tests, check-deps, e2e)
```

---

## Step 03 — container-engineer: freight POSIX ACL sandboxes

Status: **done**. Commit: `feat(daemon): freight ACL sandboxes + audit
(SPEC §4, §8.3)`.

### Done

- **`hamlet-core::freight`** (purity seam, CONVENTIONS §2) — strict
  `getfacl -p -n -c` listing parser (`parse_getfacl` → `AclListing`:
  numeric owner/group, access + default entries, `\t# effective:` stripped,
  CR rejected like the mail format) and the pure audit rule table
  (`audit(listing, AuditPolicy) -> Vec<Violation>`): owner is a charter
  slot; no `other` write (access OR default); named grantees inside the
  roster; **no named group entries at all** (group membership is not
  charter state). Typed `ViolationKind` with render-stable `kind_str`
  identities; `AclGrant::token()` renders the only `setfacl -m` spec shape
  the daemon will ever emit. Committed goldens: `parse_clean_golden`,
  `parse_mixed_golden…`, `parse_rejections_golden`,
  `audit_rule_table_golden` (CONVENTIONS §9).
- **`hamlet-daemon::freight`** (`FreightGuard`, SPEC §4/§8.3; ADR-0021) —
  consumes `Reaction::FreightDir` from the step-02 `Dispatcher`
  (registration only; zero watcher changes, as step 02 predicted):
  sequential audit task (bounded concurrency = 1). `audit()` = lstat
  (symlink/file → `not_a_directory` flag, no listing) → `getfacl` through
  `podman unshare` (userns root reads subuid-owned 0700 sandboxes; numeric
  IDs are in-ns slot UIDs) → core audit → **violations** write a
  quarantine flag `courier/freight/.quarantine/<name>.json` (atomic
  temp→fsync→rename→0444→dir fsync; daemon-owned 0755 dir: agent-readable
  evidence, never agent-writable/unlinkable) + sled trace
  (`freight-violation` under the owner's `AgentKey`). **Escalation:**
  ≥ `ESCALATION_THRESHOLD` (3) flags per owner — counted from the flag
  files (file-as-truth, restart-safe: a fresh guard re-derives the count) —
  fires the `MayorWake` hook once per owner per process lifetime
  (`RecordingWake` test seam; `StderrWake` default; Sprint 05 consumes).
- **`spawn_freight_plane`** — `ensure_root_acl()` (idempotent: quarantine
  dir, sticky root, `u:<slot>:rwx` root grants for every charter slot) +
  `audit_all()` convergence scan (sorted, restart-resumability per
  CONVENTIONS §3 r6 — dirs created while the daemon was down get flagged)
  run **before** `attach()`, then the handler registers on the shared
  dispatcher. The daemon keeps ONE tree watcher; this plane only registers
  a handler.
- **Root-ACL provisioning (the only host-side ACL writes, ADR-0021 cl. 4)**
  — `ensure_root_acl` / `grant_slot(uid)` (non-slot UIDs refused pre-side-
  effect; Mayor grantable — the manor container mounts `/var/freight/`).
  `hostperm::MODE_FREIGHT` **0710 → 0o1770**: group bits are the POSIX ACL
  *mask slot* once named grants exist (a later chmod to 0710 would clip the
  mask to r-x and silently disarm every grant); sticky stops cross-unlink/
  rename between slots sharing root rwx. Sandbox ACLs are NEVER touched
  (SPEC §4: no host-side relaxation on an agent's behalf). The four
  existing 0710 assertions (hostperm, adversarial_isolation, escape/
  provision guarded) updated; in-container "freight unreachable" assertions
  still hold wherever the plane never ran (other::--- + no grants).
- **Promotion-read helper (Sprint 06)** — `promotion_read(sandbox, file)`:
  `podman unshare cat` — host-side userns capability that reads ANY
  freight artifact regardless of ACL with zero ACL relaxation. lstat
  refuses symlinked sandboxes AND symlinked/non-regular targets *before*
  any read (SPEC §4 traversal invariant; adversarially tested).
- **`Runtime::unshare_exec`** (podman.rs) — `podman unshare <host tool>`
  seam (ADR-0011 cl. 4) with pure typed argv builders `unshare_exec_argv`
  (validate_cmd discipline), `getfacl_cmd`, `setfacl_cmd` (accepts ONLY
  `u:<digits>:[r-][w-][x-]` tokens), `cat_cmd` — goldens + injection
  matrix inline. `FakeRuntime` implements it with the existing
  `script_exec_for` matcher (ACL listings as fixtures) and verb-targeted
  failure injection (`fail_next_for("unshare", …)`).
- **Guarded podman suite** `crates/hamlet-daemon/tests/freight_guarded.rs`
  (`#[ignore]` + `HAMLET_PODMAN_TESTS=1` + podman-on-PATH + host
  setfacl/getfacl/cat preflight, skip reason recorded; ci.sh step-5 filter
  matches): provisions THREE real parcels (A=1001, B=1002, C=1003), then
  A `mkdir /var/freight/exchange` + `setfacl -m u::rwx,u:1002:rwx,o::---`
  → B reads AND writes → C denied read/write/list/`setfacl`-rewrite/
  `rm -rf`/`mv` (sticky) while still seeing root names → real daemon audit
  calls it Clean → `chmod 0777 openbox` flagged WorldWritable (flag 0444
  on host; C reads it in-container, cannot forge/unlink it; sled traced) →
  `setfacl -m u:4242:rwx rogue` flagged ForeignUid → `ln -s /etc evil`
  flagged NotADirectory without a listing → third A-owned violation trips
  the wake hook with the sorted evidence list → `promotion_read` succeeds
  on exchange AND openbox (0700/0777 alike), refuses the symlink sandbox,
  and relaxes nothing (B still reads, C still denied, re-audit still
  Clean). **Not executed on this host — podman is not installed here**
  (guard skips with recorded reason); first real run is the podman-capable
  CI job.
- Tests: `tests/freight.rs` **17** (FakeRuntime + fixtures: root-grant
  argv golden/idempotence, non-slot refusals with zero podman calls, the
  four audit rules, closed-commission grantees, symlink-without-listing,
  vanished-dir no-op, corrupt-listing/failing-tool typed errors, threshold
  escalation once + restart-safe counts across guards, promotion-read
  golden + symlink refusals + tool failure, plane startup convergence +
  watcher-driven event audit, audit_all report incl. sanitizer-skips);
  core **+6** freight goldens + 3 new tree accessor goldens; podman +2
  argv goldens/injection.

### Decisions

- **ADR-0021 (`docs/adr/0021-freight-acl-sandboxes.md`, amends ADR-0011
  cl. 3 freight row; README index updated)** — the prompt's
  "commission-scoped sharing" policy: **grantee roster = Mayor (1000) +
  slots bound to an Open commission**; owner set = every charter slot.
  Closed-commission and idle slots are not shareable (a slot's commission
  lives in `charter.slots[uid].commission` until Sprint 06 introduces
  per-commission freight scoping — the audit consumes an injected
  `AuditPolicy`, so narrowing is a roster-set change only). Also pinned:
  root mode 0o1770 rationale (mask slot + sticky), quarantine flag
  location/format/0444, escalation threshold 3 (const, not config),
  promotion-read via `unshare cat` with pre-read symlink refusal.
- **Blueprint: NO version bump — the prompt's premise was already
  satisfied.** `defaults/village-alpine.toml` v1 has carried the `acl` apk
  tool since Sprint 02 ("setfacl/getfacl for /var/freight/ sandboxes");
  the in-container tooling path needs nothing more. Per ADR-0009 the
  blueprint version is "bumped only by the Sprint 06 promotion path", and
  the derived-image tag embeds it (ADR-0012: `v<bp>-f<floor>-c<fp>`) — a
  content-free bump would retag/rebuild every parcel image for zero
  functional change. Recorded here as the step's blueprint decision.
- **Escalation dedupe:** threshold counts are file-as-truth (flag dir
  scan); the once-per-owner dedupe is in-process only — a restarted daemon
  MAY re-fire for an already-escalated owner with a new violating dir.
  Acceptable for a wake *hook* (waking the Mayor is idempotent-ish: a
  fresh single-turn packet); Sprint 05 may persist last-escalated state if
  it needs stricter once-semantics.
- **Clean re-audits never remove flags:** quarantine evidence persists
  until Mayor/operator resolution (Sprint 05+ decree path); a re-violation
  overwrites with fresh evidence. Prevents flag flapping as agent fix-ups
  race audits.

### Open issues / carry-forward

- **Sprint 05 (Mayor loop)** consumes `MayorWake`/`FreightEscalation`
  (`hamlet-daemon::freight`) — build the wake context packet from the
  quarantine flag files (they are agent-readable too) and decide the
  flag-clearing decree vocabulary.
- **Sprint 06 (promotion)** consumes `promotion_read` (and may want a
  `promotion_list` sibling — deliberately NOT added now: unused surface).
  Vault copies stay daemon-performed with every name through
  `hamlet-core::sanitize` (SPEC §11.3); the symlink refusals here are the
  host-side half of the traversal invariant.
- **Sprint 07 (census/provisioning)** should call
  `FreightGuard::grant_slot(uid)` after `allocate_slot` (mirrors the
  step-02 `ensure_spool_dirs` advisory); `ensure_root_acl` reconciles all
  charter slots at every plane start regardless.
- **Bare files at the freight root:** `audit_all` flags them
  `not_a_directory`, but the creation *event* (`classify`) only reacts to
  dirs — a file dropped at the root is caught at the next plane start, not
  live. QA (step 05) may want an assertion; a watcher-table change would
  be an ADR-0019 amendment.
- **Host prerequisites (guarded CI job):** `acl` (setfacl/getfacl) +
  coreutils `cat` on the daemon host — the freight_guarded preflight skips
  with a recorded reason when missing; e2e-02's subuid preflight still
  applies for provisioning.

### Verify

```sh
cargo test -p hamlet-core --lib freight   # 6: parser + audit-rule goldens
cargo test -p hamlet-core --lib tree      # accessor goldens incl. quarantine paths
cargo test -p hamlet-daemon --lib podman  # unshare argv goldens + injection matrix
cargo test -p hamlet-daemon --test freight          # 17: FakeRuntime audit/escalation/promotion/plane
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon --test freight_guarded -- --ignored podman  # guarded (skips w/o podman, reason recorded)
./scripts/ci-local.sh                     # CI battery: PASS (fmt, clippy -D warnings, workspace tests, check-deps, e2e)
```

---

## Step 04 — daemon-engineer: dispatch outbox/inbox + axum inbound webhooks

Status: **done**. Commit: `feat(daemon): webhook dispatch outbox/
inbox + axum inbound endpoints (SPEC §8.4–§8.5)`.

### Done

- **`hamlet-daemon::dispatch`** (SPEC §8.4; ADR-0020 protocol, ADR-0023
  runtime) — `DispatchPlane` consumes `Reaction::DispatchOutbox` from the
  shared step-02 `Dispatcher` (registration only; zero watcher changes):
  validate against the ADR-0020 schema → config allowlist check → resolve
  `headers_env` secrets **from `.env` by key name at send time, per attempt**
  (`load_env_secrets`: dotenvy pure iterator, the single `.env` read seam,
  never cached) → POST via injected `HttpClient` (production:
  [`ReqwestClient`] = reqwest rustls-tls + json + config timeout; tests:
  scripted seam) → atomic inbox file. Retryable failures (429/5xx/transport)
  **checkpoint through the inbox file** before sleeping on the deterministic
  jitter-free schedule; a restart resumes at `attempt + 1` — the budget is
  per file, not per process lifetime. Exhaustion or validation failure
  (schema, oversize, allowlist, missing `.env` key — zero network attempts)
  → final error inbox + rename into `dead/`. Delivery removes the outbox
  file: **the inbox *is* the record**. `dispatch.enabled=false` freezes
  files untouched. `spawn_dispatch_plane`: sorted recovery scan strictly
  before handler attach (restart-resumability, CONVENTIONS §3 r6).
  Concurrency: one coordinator task, one worker per correlation,
  in-flight dedupe set (process-local; the tree dedupes across restarts).
  Sled trace per attempt and terminal (`dispatch-attempt` / `-delivered` /
  `-dead`: correlation + attempt + method + URL authority only — never
  headers, body, query, or secrets).
- **Opaque transport tokens** — reqwest's `Display` embeds the URL (query
  strings may carry secrets); failures are classified into four fixed ASCII
  tokens before they can reach inbox files or sled (CONVENTIONS §6).
- **`hamlet-daemon::web`** (SPEC §8.5; ADR-0022) — `WebhookService::accept`
  is the only logic path (axum handlers are thin wrappers): §4 sanitizer on
  the raw topic → config routing table → body-cap re-check → auth
  (`X-Hamlet-Auth` vs `.env` key via no-early-exit `secret_eq`; missing/empty
  key fails **closed** with 503) → JSON validation + canonical re-render
  (sorted keys, CONVENTIONS §3.1) → correlation from the durable sled
  counter (`d-<seq>-1000.0`, monotonic across restarts, no wall-clock)
  → sinks: dispatch **inbox** envelope (`attempt: 1`, **`status: 202`**
  daemon-accepted marker, empty headers, ADR-0020 cl. 2 cap + truncation via
  the storage constructor) and/or mail to route recipients (`MailActor::`
  `Patron("webhook")` — daemon-asserted identity, normal bounce rules)
  → `WebhookWake` hook (registered, inert until Sprint 05) + one sled trace.
  Rejection classes leave the tree untouched: 404 (disabled / sanitizer-
  failing / unconfigured topic — one indistinguishable class), 401, 503,
  413 (tower-http `RequestBodyLimitLayer` at hyper level + service re-check),
  400. `spawn_web_server`: binds `web_listen_addr`, graceful shutdown;
  disabled config never binds. **No orchestration in handlers**: validate →
  write files → emit hook; downstream reactions flow through the existing
  watcher/router planes (the spool file the mail sink writes is routed like
  any other incoming mail).
- **Config v6** (additive): `webhooks` block (`enabled=false` default,
  `auth_env_key`, `max_body_bytes` 1..=1 MiB, `routes` table) +
  `validate()` (env-key grammar via new core `is_env_key`, topic sanitizer,
  ≥1 sink per route, slot uids only, no duplicates). v5 documents parse with
  deny-all defaults. Goldens updated (core render golden + CLI golden
  config/env-template); ENV_TEMPLATE gains commented `HAMLET_WEBHOOK_AUTH`
  / `HAMLET_DISPATCH_AUTH` examples (obvious fakes only).
- **ADRs**: ADR-0022 (`docs/adr/0022-inbound-webhooks.md` — inbound auth
  shape, routing table, 202 inbox envelope) + ADR-0023 (`docs/adr/
  0023-dispatch-runtime-checkpoints.md` — dispatch runtime: inbox-file
  attempt checkpoints, restart resume, idempotent recovery, per-correlation
  workers); README index updated.
- **Ratchets re-scoped as confinement rules** (e2e-02 T2, e2e-03 T3):
  SPEC §8.4/§8.5 mandate daemon-side `.env` reads ("endpoint auth via
  `.env`") — the pre-existing zero-tolerance ratchets predate this step.
  Now: `env_file()` confined to `dispatch.rs` (`load_env_secrets`, shared by
  both planes); `.expose()` confined to `supervise.rs` (session env channel)
  + `dispatch.rs` (wire headers at send time) + `web.rs` (auth compare).
  Any new unmasking site fails the gate until reviewed.
- **Tests**: `tests/dispatch.rs` **19** (round-trip incl. secret-by-key +
  correlation end-to-end + no-secret-in-artifacts, permanent-4xx-delivered,
  retry→success, transport retry, dead-letter on exhaustion, the five
  validation classes with zero network attempts, restart checkpoint
  resume/exhaustion/stale-outbox recovery, idempotent re-processing,
  disabled-plane freeze, recovery scan incl. foreign-name skips, watcher-
  driven e2e, tick-by-tick backoff) and `tests/web.rs` **12** (accepted →
  inbox+mail+hook+trace, canonical sorted JSON, mail-only routes, every
  rejection class with a whole-tree-unchanged assertion incl. traversal-
  shaped topics, disabled never binds, correlation monotonicity across a
  simulated restart, oversized-event truncation at both caps).

### Decisions

- **ADR numbering:** the prompt cites ADR-0010 — read as **ADR-0020** (step
  01 renumber: dispatch protocol). The *runtime* contract (where attempt
  state lives, restart resume, double-send prevention) is new **ADR-0023**;
  the inbound surface (auth/routing/envelope) is new **ADR-0022**.
- **The inbox file is the attempt checkpoint** (file-as-truth): no sled
  dispatch state. Every state is derivable from `courier/dispatch/` alone —
  outbox present + no checkpoint = unsent; error checkpoint = mid-retry;
  http checkpoint = delivered; `dead/` = terminal. Kill-the-daemon-at-any-
  point converges: ≤ `max_attempts` total sends per file.
- **202 status marker** distinguishes daemon-originated inbound inbox files
  from real upstream responses (same ADR-0020 envelope, no schema change).
- **`WebhookWake` mirrors `crate::freight::MayorWake`**: registered fire-
  and-forget hook seam; `StderrWebhookWake` default until Sprint 05 step 03
  wires wake triggers (OBJECTIVE lists "webhook events" as a wake source).
- **tower-http** (feature `limit`) joins the SPEC §3 runtime allowlist via
  ADR-0022: axum's `DefaultBodyLimit` does not apply to the raw `Bytes`
  extractor in axum 0.7, so the cap must live below the extractors.
- **Corrupt checkpoint = absent** (`read_checkpoint`): re-process from
  attempt 1 rather than trust broken state — checkpoints are daemon-written,
  so this is unreachable without external tampering; worst case is bounded
  extra sends on an already-tampered tree.

### Open issues / carry-forward

- **Sprint 05 (Mayor loop)** consumes `WebhookWake` (`hamlet-daemon::web`)
  alongside freight's `MayorWake`: build wake context packets from the inbox
  envelope/mail artifacts and classify decree-relevant triggers; replace
  `StderrWebhookWake` with the real wake.
- **Binary assembly:** `main.rs` remains a stub through this sprint (steps
  02–04 ship planes as library spawn functions over the shared `Dispatcher`,
  matching steps 02/03); the daemon boots all four planes + web server when
  Sprint 05 wires the wake loop.
- **WS metrics/chat** (SPEC §12.3) lands in Sprint 09 on this same axum
  server surface (`web.rs` is the only HTTP surface; `spawn_web_server`'s
  graceful-shutdown handle is already reusable).

### Verify

```sh
cargo test -p hamlet-daemon --test dispatch   # 19: round-trip, retry/dead-letter, validation classes (zero network), restart checkpoints, disabled freeze, recovery scan, watcher e2e, backoff determinism
cargo test -p hamlet-daemon --test web        # 12: accepted inbox+mail+hook+trace, all rejection classes (auth/oversize/malformed/traversal — tree untouched), disabled never binds, restart monotonicity, truncation
cargo test -p hamlet-core config              # v5→v6 additive migration, webhook routing validation
cargo test -p hamlet-daemon --lib             # in-module unit tests (seams, tokens, caps)
./scripts/ci-local.sh                         # CI battery: PASS (fmt, clippy -D warnings, workspace tests, check-deps, e2e incl. re-scoped ratchets)
```

## Step 05 — qa-engineer: adversarial comms suite

### Done

- **Committed adversarial suite** `crates/hamlet-daemon/tests/adversarial_comms.rs`
  (27 tests, all green; no podman required — scripted seams only):
  - **A. Mail format:** CR/CRLF/space injection matrix at the parse wall
    (`injection_matrix_cr_newlines_and_spoofed_from`), exact-bound oversize
    fields (`oversized_fields_rejected_at_exact_bounds`).
  - **B. Spoofed From vs daemon-asserted identity:** delivery under claimed
    identity, bounce targeting (QA04-01 pin), daemon send API asserts actor
    identity from the caller — no caller input can claim a `From:`.
  - **C. Bounce paths:** unallocated / hibernating / crashed / overflow
    matrix; named-sender bounces die without chaining.
  - **D. Partial files:** writer crash mid-rename — `.tmp` never routed,
    survivor file is what gets delivered.
  - **E. Kill/restart:** three daemon lives over one tree (kill -9 between);
    zero duplicates, zero drops, FIFO order; second restart scans nothing.
  - **F. FIFO storm:** 3 writers × interleaved seqs — deterministic
    lexicographic scan order, all delivered exactly once.
  - **G. Watcher:** churn blips (create/delete) never react; downtime files
    recovered after restart; burst reactions sequenced monotonically.
  - **H. Freight:** audit-rule matrix (owner-not-slot / foreign-uid /
    named-group / world-writable, single + combined, deterministic order);
    symlink escape flags `NotADirectory` without a getfacl call; promotion-
    read works under strict ACLs (FakeRuntime).
  - **I. Dispatch:** allowlist matrix (exact host/port, byte-prefix path,
    `http://` denied at config time), secrets never in outbox/inbox/sled,
    response-header redaction, retry schedule pure-function check + wall-
    clock gap sampling, dead-letter after exhaustion, malformed JSON →
    precise error file with the plane staying alive.
  - **J. Inbound webhooks:** bad-auth variants (401, tree untouched),
    auth-env missing fails closed (503), oversize body 413 at both layers,
    traversal topics 404 + sanitizer-clean artifacts, content-type smuggling
    (body is truth — raw socket, no client normalization), rejection
    artifacts never echo input.
- **Harness** `scripts/e2e-04-comms.sh` (new): T1 adversarial suite, T2
  source-level ratchets (CR rejection in mail.rs; `secret_eq` + fixed
  rejection tokens in web.rs; REDACTED_HEADERS family in dispatch.rs;
  quarantine 0444/0755 constants pinned), T3 guarded podman freight suite
  (skips cleanly without `HAMLET_PODMAN_TESTS=1`; with it set, the guarded
  tests' internal `podman --version` guard skips on this host — see QA04-04).

### Findings

- **QA04-01 (LOW, observation pinned) — spoofed-From bounce asymmetry.**
  Well-formed mail: the charter cross-check is slot-existence only
  (ADR-0019 cl. 2), so delivery honors the claimed `From:`; the *bounce*
  for a rejected well-formed envelope also targets that header. Malformed
  mail has no usable headers and bounces to the **file-name writer**
  (daemon-asserted). Consequence: an agent forging `From: <other>` in a
  conforming-named file gets its rejection notice at `<other>`'s door —
  inbox noise for another slot, no delivery privilege gained, no data
  loss. The `SpoolRef.writer` doc says "the daemon cross-checks file
  owner"; no such cross-check exists for well-formed mail. Ruling: the
  ADR-0019 trust model intentionally trusts conforming mail, so this is a
  documented design consequence, not a defect — but it is an asymmetry
  worth one line in the ADR. Pinned by
  `spoofed_from_bounce_targets_header_not_writer` (asserts the exact
  header-vs-writer split) so any later hardening shows up as a deliberate
  diff.
- **QA04-02 (INFO) — allowlist byte-prefix is per-spec but over-matches.**
  ADR-0020 cl. 3 match = exact host + exact port + **byte-prefix path**:
  entry `/v1` admits `/v1evil/x`. Intentional per the ADR; noted so that
  future entries are written with separator hygiene (`/v1/`) when path-
  scoping is intended. Pinned by `allowlist_matrix_exact_authority_and_prefix`.
- **QA04-03 (test-infra, disclosed) — sled try-lock race under test load.**
  The kill/restart simulation flaked (~1-in-8) with `WouldBlock` on the
  third telemetry reopen: without its `testing` feature, sled locks with
  `try_lock_exclusive()` and documents exactly this race. Fixed test-side:
  `sled = { features = ["testing"] }` added to hamlet-daemon **dev**-deps
  (blocking lock in test builds only; production builds keep the workspace
  dep unchanged). Not a product change — disclosed per role constraint.
- **QA04-04 (CI-host, RESOLVED) — podman is now usable; the guarded
  battery runs and surfaces real findings.** The binary permission was
  fixed (`755`) and, after a reboot, the `tun` kernel module loaded —
  buildah/pasta work and images actually build (verified: alpine pull +
  RUN steps succeed). With that, the sprint-02/03 guarded battery ran for
  the first time on this host and its failures are no longer
  environmental — they are the product/host findings QA04-05…07 below.
  Residual note: `ci.sh` step 5 still keys on PATH presence; a half-
  working podman turns a SKIP into an environmental RED (carry to CI-agent).
- **QA04-05 (HIGH, product · sprint 02 scope) — node musl build needs
  libstdc++, which the alpine floor does not install.** The pinned runtime
  `node-v22.23.3-linux-x64-musl.tar.xz` (sha256-verified against the
  official unofficial-builds artifact — network is clean) is a musl binary
  that is *dynamically linked* against `libstdc++.so.6`/`libgcc_s.so.1`
  (V8's C++ runtime). The village-alpine floor (`acl ca-certificates curl
  git gzip tar uutils-coreutils xz`) installs no libstdc++, so inside real
  containers the node probe dies: `Error loading shared library
  libstdc++.so.6` → bootstrap aborts (exit 127). Blocks every guarded
  provisioning flow on any podman-capable host: freight, bootstrap-
  toolchain and provision suites all die at or before the node probe.
  Fix (product): add `libstdc++` to the blueprint apk floor — note this
  changes the Containerfile fingerprint → new derived-image tag.
- **QA04-06 (MEDIUM, product · sprint 02 scope) — alpine:3.24 upstream now
  ships `CMD ["/bin/sh"]`.** The current upstream base (built 2026-09-17)
  carries an explicit `CMD` layer; the golden Containerfile never overrides
  it, so the derived village-alpine image inherits `Cmd=[/bin/sh]` and the
  pinned invariant "the derived image declares no entrypoint/cmd (the
  daemon supplies the keep-alive)" fails. Fix (product): add an explicit
  `CMD []` to the golden Containerfile (same fingerprint/tag note as QA04-05).
- **QA04-07 (MEDIUM, CI-host deployment) — the guarded battery assumes a
  daemon user disjoint from the container userns.** Three distinct checks
  require it: (a) escape suite asserts the daemon's host uid falls outside
  *every* row of the container `uid_map` — on this host the test/daemon
  runs as `dock`(1000), which owns its own rootless containers, so the
  owner row (`0 → 1000`) collides by construction; (b) provision asserts
  the parcel home is subuid-owned after `podman unshare chown` — here the
  home stays daemon-owned (restricted userns stack: tmpfs `/dev`, minimal
  module set); (c) supervise's smoke fixture creates the home as the daemon
  user, which appears as container-root inside → uid-1001 writes get EACCES.
  Fix (host): a dedicated `hamlet` service user whose uid is outside the
  subuid range *and* not the podman owner, on a host with full rootless
  userns/subuid support. No product change implied.

### Decisions

- **All five prompt attack categories hold at unit level.** Mail / watcher /
  dispatch / inbound webhooks verified end-to-end over scripted seams; freight
  verified via FakeRuntime (audit matrix, symlink escape without getfacl,
  promotion-read under strict ACLs). Every rejection is typed and sanitizer-
  clean; the plane stays alive after malformed input.
- **The guarded real-container freight verification could not complete** on
  this host: podman now works (QA04-04 resolved) but the provisioning it
  depends on is blocked by sprint-02 product findings QA04-05 (node/libstdc++
  — HIGH) and QA04-06 (alpine CMD inheritance), plus host-deployment QA04-07.
  None are Sprint 04 defects; all five planes' committed adversarial tests
  remain green.
- **Verdict:** `GATE 04/05: PASS` — 27/27 adversarial tests green across
  repeated runs, workspace battery green (349+ tests), e2e harness PASS
  (T1 + T2). T3 (guarded freight) ran but is blocked by QA04-05/06/07 and
  carries to the sprint-02 owners + a compliant CI host — same treatment as
  gates 02/06 and 03/06, now with concrete blockers instead of absence.

### Open issues / carry-forward

- **Sprint 05 (Mayor loop):** `WebhookWake` consumers should treat the
  fixed rejection tokens as an API (they are sanitizer-clean by design);
  if bounce-targeting hardening is ever wanted (QA04-01), the pinned test
  defines the exact behavior change to review.
- **QA04-05/06 land on the sprint-02 owners (bootstrap/blueprint):** add
  `libstdc++` to the apk floor and an explicit `CMD []` to the golden
  Containerfile; both change the image fingerprint, so re-pin the derived-
  image tag. Until then every guarded provisioning flow fails at the node
  probe / no-entrypoint invariant on *any* podman-capable host.
- **QA04-07 lands on the CI-agent/host:** dedicated `hamlet` service user
  (uid outside subuid range, not the podman owner) + full rootless userns/
  subuid support; then re-run T3 of `scripts/e2e-04-comms.sh` and the full
  guarded battery.
- **QA04-04 residual:** decide whether `ci.sh` step 5 should probe *usability*
  (`podman --version`) rather than mere PATH presence, so a half-working
  podman doesn't turn a SKIP into an environmental RED.

### Verify

```sh
cargo test -p hamlet-daemon --test adversarial_comms   # 27: A–J matrix (above)
./scripts/e2e-04-comms.sh                              # T1 suite + T2 ratchets; T3 guarded runs when HAMLET_PODMAN_TESTS=1
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon --test freight_guarded -- --ignored   # blocked by QA04-05/06/07 until fixed
cargo test --workspace                                 # full battery green incl. new dev-dep feature
```

---

## Sprint 04 fix pass — container-engineer: QA04-05 + QA04-06 (image policy)

Status: **done** (fixes the two product blockers step 05 handed to the
container layer). Both were reproduced and root-caused on a real podman host
before changing anything; both fixes are verified against real containers.

### Done

- **QA04-05 — the pinned musl toolchain had no C++ runtime (HIGH).**
  Root cause confirmed by inspection, not inference: `readelf -d` on the pinned
  `node-v22.23.3-linux-x64-musl.tar.xz` shows `DT_NEEDED` on **`libstdc++.so.6`
  AND `libgcc_s.so.1`**, and `alpine:3.24` ships *neither* (a `find` for both in
  the base image returns nothing). Note the finding named only `libstdc++`;
  fixing just that would still leave node dying on `libgcc_s.so.1`, so the floor
  gains **both**. Fix: `BOOTSTRAP_FLOOR` v1 → v2 =
  `ca-certificates curl gzip libgcc libstdc++ tar xz`, `FLOOR_VERSION` 1 → 2
  (ADR-0024). They are **floor** packages, not blueprint ones: the requirement
  comes from the daemon's *pin table* (an artifact property), so a promoted
  blueprint must not be able to drop them by omission. Blueprint `version` stays
  1 — ADR-0009 cl. 1 bumps it only on promotion; the floor has its own version
  axis, and the bump is what forces the rebuild (the tag is
  `v<bp>-f<FLOOR_VERSION>-c<fingerprint>`).
- **QA04-06 — the derived image inherited its parent's body (MEDIUM).**
  Confirmed: `podman inspect alpine:3.24` → `Config.Cmd = ["/bin/sh"]`, and
  Containerfile instructions are *additive*, so "no CMD line" never meant "no
  CMD" — every parcel image built since Sprint 02 carried an inherited root
  shell. It was unreachable (the run argv always appends an explicit non-empty
  command, `validate_cmd` refuses an empty one), but the pinned invariant was
  false. Fix: the generated Containerfile now **explicitly clears** both —
  `ENTRYPOINT []` and `CMD []` (ADR-0025), which holds for *any* permitted
  parent instead of depending on a parent shipping none. Verified fail-closed:
  `podman run --rm <tag>` with no command now **errors** (exit 127, "executable
  file not found") instead of quietly starting `/bin/sh`; explicit commands and
  the `sleep infinity` keep-alive still work.
- **Image tag re-pin is automatic and done:** the definition hashes to
  `localhost/hamlet/village-alpine:v1-f2-c0a8b8a8b1160d5c8`. Floor-v1 images
  are superseded (orphaned `localhost/` tags; `podman rmi` housekeeping).
  DEED-baseline and `.profile` goldens updated to the new tag + apk list.
- **QA04-04 residual fixed:** `scripts/ci.sh` step 5 now probes podman
  *usability* (`podman info --format '{{.Host.OCIRuntime.Name}}'`) instead of
  PATH presence, so a half-working podman produces a recorded SKIP rather than
  an environmental RED.
- **Tests.** Unit: the Containerfile golden (now four instructions), a new
  `containerfile_always_clears_entrypoint_and_cmd` (asserts the *intent*
  independently of the golden text, so a future rewrite can't drop the resets),
  and `floor_carries_the_musl_runtime_libraries`. Guarded (real podman, both
  green here): `podman_musl_runtime_libs_present_root_owned_and_not_agent_writable`
  and `podman_derived_image_inherits_no_body_from_its_parent`.
- **Ratchets** (`scripts/e2e-02-isolation.sh` T2, source-level): the floor
  contents must equal the blessed floor-v2 set *and* `FLOOR_VERSION` must be 2
  — a floor edited without a version bump fails the gate, because the tag would
  not move and stale images would be served forever (the exact QA04-05 failure
  mode). Plus a source-level pin that the generator still emits both clears.

### Security review of the new capability

The fix puts two shared libraries in every parcel rootfs, so "the agent cannot
tamper with the toolchain" becomes a real invariant rather than an omission.
`podman_musl_runtime_libs_present_root_owned_and_not_agent_writable` asserts it
adversarially on a real container: both libs are **root-owned** with no
group/other write bit, and as a slot UID (`--user 1001:1001`, `--cap-drop=ALL`)
an agent fails to `rm`, symlink-shadow, overwrite, or `chmod 0777 /usr/lib` —
each attempt asserted to fail, with the originals verified intact afterwards.
Loader hijack stays impossible from inside a parcel: `LD_LIBRARY_PATH` /
`LD_PRELOAD` / `LD_AUDIT` remain in `FORBIDDEN_ENV_KEYS` (ADR-0014) and the
container cannot chown root-owned files.

### Decisions

- **Two ADRs, not one** (house rule: one decision per file): **ADR-0024**
  (musl runtime libraries in the floor) and **ADR-0025** (the derived image
  clears its parent's ENTRYPOINT/CMD). Both amend **ADR-0012 cl. 2** only;
  its status line and the README index record the amendment.
- **Floor, not blueprint, for the runtime libs** (see above) — this is the
  distinction that matters for promotion: ADR-0013 lets any agent UID publish
  into the pkgcache, so *declared* packages are weaker than *pinned* ones.
- **`USER` stays omitted** (not cleared) in the Containerfile — it is not
  inheritance-sensitive in the same way and the run argv's `--user` is the
  authority; a `USER` line would be a spec change to the new clause.

### Findings raised by this pass

- **QA04-08 (HIGH, host · CI-agent) — the guarded cold-bootstrap suite cannot
  complete on *this* host: it runs out of RAM, not podman.** With QA04-05/06
  fixed, `podman_bootstrap_installs_toolchains_and_reprovisions_with_zero_downloads`
  now gets *past* the node probe (that alone proves QA04-05 fixed — the plan
  reaches the rustc step only after node is downloaded, digest-verified,
  extracted and probed) and then dies extracting rustc. Two observed failure
  modes of the same cause: `tar: Unexpected EOF in archive` (exit 2), and on a
  re-run **`exit 137` = SIGKILL**. Evidence: `/tmp` here is a **7.5 GB tmpfs
  (RAM-backed) at 72–81 % full with ~1.5–2.6 GB free**, the host has
  **~2.2 GB of 15.3 GB RAM available**, and the suite's cold mirror writes
  ~200 MB of archives plus ~1 GB of extracted toolchains into that tmpfs under
  `--memory 512m`. A container in the identical environment with an identical
  download→verify→extract sequence succeeds when the tmpfs has room. Not a
  product defect and not reproducible on a host with a disk-backed `TMPDIR` for
  the volume reason — but note the disk-backed variant then hits the next item.
  **Fix (host):** give the guarded job a disk-backed `TMPDIR` and RAM headroom
  (≥ 4 GB free), which is the same class of host provisioning as QA04-07.
- **QA04-09 (MEDIUM, test · container/QA) — two guarded-bootstrap assertions
  assume the parcel home and the pkgcache mirror share a filesystem.** With a
  disk-backed `TMPDIR` the run gets further and every publish fails with
  `ln: … Cross-device link`, because SPEC §4 mounts them as two *separate* bind
  mounts — so the hardlink publish in ADR-0013 cl. 3 **cannot** succeed there,
  and the suite's `publish_failures.is_empty()` plus its later
  `entries.len() == ARTIFACT_COUNT` mirror assertion fail by construction.
  ADR-0013 cl. 3 explicitly calls EXDEV publish non-fatal ("it is a cache"),
  so the code is right and the *assertions* are host-shape-dependent. I did not
  relax them: the follow-on claim they protect (a second parcel installs with
  zero downloads) is only true when the mirror really is populated, and
  loosening both would hide a genuine degradation. Needs a QA decision — either
  "populated OR every failure is EXDEV" (and then the zero-download claim must
  be conditional too), or a host whose home+pkgcache share a device.

### Open issues / carry-forward

- **QA04-07 still open** (unchanged, host): dedicated `hamlet` service user,
  uid outside the subuid range and not the podman owner. On this host the
  daemon *is* the podman owner (`dock`/1000), which is why the rollback path
  reports `ROLLBACK INCOMPLETE: … Permission denied` and why `podman unshare`
  is needed to delete a leaked village tree.
- **QA04-08/09** land on the CI-agent (host shape) and QA (assertion policy);
  QA04-08 in particular means the guarded battery is still not a green gate on
  this host, though the *specific* blockers QA04-05/06 named are fixed and
  their guarded tests pass.
- **No blueprint change**: `defaults/village-alpine.toml` stays at v1 — the
  floor has its own version axis, and the fingerprint (not the version) is what
  moved. A future pinned artifact with further `DT_NEEDED` entries follows the
  same path: reviewed floor edit + `FLOOR_VERSION` bump + the blessed set in
  the e2e-02 ratchet, in one commit.

### Verify

```sh
cargo test -p hamlet-daemon --lib bootstrap          # goldens + the two new floor/body unit tests
./scripts/e2e-02-isolation.sh                        # T2 now ratchets floor-v2 + the CMD/ENTRYPOINT clears
HAMLET_PODMAN_TESTS=1 cargo test -p hamlet-daemon --test bootstrap_guarded -- --ignored \
  podman_musl_runtime_libs podman_derived_image_inherits_no_body    # both green on a real podman host
cargo clippy --workspace --all-targets -- -D warnings && ./scripts/check-deps.sh   # clean
./scripts/ci-local.sh                                # steps 1-4 + all e2e PASS; step 5 guarded red only on this host (QA04-08)
```

---

## Step 05 — qa-engineer (verification pass): mutation testing the adversarial suite

Status: **done**. The step-05 suite above is green, but "the tests pass" is not
the same claim as "the tests *can* fail." This pass attacks the suite itself.

### Done

- **Mutation testing of the committed adversarial suite.** Six targeted
  defects injected into product code, one at a time, each reverted immediately:
  | # | Injected defect | Caught by `adversarial_comms`? |
  |---|---|---|
  | MUT1 | CR rejection deleted in `hamlet-core/src/mail.rs:217` | **yes** — `injection_matrix_…` |
  | MUT2 | `PathSeparator` + `DotName` sanitizer arms deleted | no — **benign**, see below |
  | MUT3 | world-writable rule deleted in `hamlet-core/src/freight.rs:373` | **yes** — `audit_rule_matrix_…` |
  | MUT4 | allowlist enforcement call deleted in `dispatch.rs:402` | **NO → QA04-10** |
  | MUT5 | ack skipped on the happy path in `mailroute.rs:440` | **yes** — 2 tests (+7 in `mailroute`) |
  | MUT6 | jitter added to the backoff sleep in `dispatch.rs:485` | **NO → QA04-11** |
  MUT5's blast radius is the headline for the suite's core claim: removing a
  single `self.ack(...)` line turns `kill_restart_zero_duplicates_across_two_
  restarts` and `fifo_storm_mixed_writers_deterministic_order` red and 7 more
  in `mailroute`, so "zero duplicate delivery" is genuinely pinned.
- **MUT2 is benign and worth recording as a positive.** Deleting both the
  `PathSeparator` and `DotName` arms changes nothing observable, because the
  `SHAPE` regex (`^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)?$`) independently rejects
  `.`, `..` and separators. The sanitizer is defense-in-depth; the suite pins
  the *outcome* (the right level) and the 18 `SanitizeError` variant assertions
  elsewhere pin the arms. No finding.
- **Two new regression tests** (29 total, all green):
  `disallowed_url_is_refused_before_any_network_attempt` (port-mismatch,
  foreign host and lookalike subdomain each dead-letter with **zero** HTTP-seam
  calls, dead-letter bytes intact, no secret in the artifact) and
  `backoff_gaps_never_exceed_the_deterministic_schedule` (gaps must equal
  `RetryPolicy::schedule()` within scheduler slack, upper-bounded).
- **Two new T2 source ratchets** in `scripts/e2e-04-comms.sh`, because a
  wall-clock tolerance cannot pin millisecond determinism (see QA04-11) and a
  source pin is the only deterministic enforcement: the `allowlist.allows()`
  enforcement call must exist, and the backoff sleep must remain the bare
  `sleep(Duration::from_millis(ms))` with no arithmetic on `ms`.
- **Coverage note (prompt item 1, "FIFO under 1k-message storm").** The 1k
  storm is `storm_1k_spool_writes_all_delivered_fifo` in `tests/mailroute.rs`
  (100 × 10 recipients, real temp+rename writes), not in this suite — this
  suite's storm test is 3 writers × 20 (order determinism). Both exist; the
  1k requirement is met by the mailroute test.

### Findings

- **QA04-10 (MEDIUM, test · QA) — the adversarial suite never exercised the
  dispatch allowlist *enforcement*, only the pure matcher.** Deleting
  `if !allowlist.allows(&request.url)` (`crates/hamlet-daemon/src/dispatch.rs:402`)
  left this file **27/27 GREEN**: `allowlist_matrix_exact_authority_and_prefix`
  constructs a `hamlet_core::dispatch::UrlAllowlist` directly and asserts only
  the matcher's return values, so the plane's refusal path was untested here.
  `tests/dispatch.rs` *did* catch it (`validation_rejects_disallowed_url`,
  `recover_outbox_report_mixes_outcomes`), so SPEC §8.4 was never unenforced —
  the gate was covered, but by a different suite than the one whose stated
  remit is "non-allowlisted URL refused", and a reader of the adversarial suite
  alone would have drawn a false conclusion about its coverage. Fixed
  test-side: added the plane-boundary regression test + a T2 ratchet on the
  enforcement call; both verified red under MUT4.
- **QA04-11 (MEDIUM, test · QA) — the ADR-0020 cl. 4 "deterministic,
  jitter-free" backoff was unpinned at the call site.** Adding
  `let ms = ms + (attempt as u64 % 7) * 3;` before the sleep
  (`crates/hamlet-daemon/src/dispatch.rs:485`) kept the **entire workspace
  green — all 36 test binaries**. Two holes compounded: (a) the only exact
  schedule assertions were on the pure `RetryPolicy::schedule()`
  (`retry_schedule_is_a_pure_function`), which the mutation does not touch;
  (b) the only runtime assertion, `retry_wall_clock_gaps_and_dead_letter_budget`,
  checks **lower bounds only** (`>= 95`, `>= 195`), so any *additive* delay
  passes. Wall-clock tolerances cannot pin ms-scale exactness without
  flaking on load, so the durable fix is the T2 ratchet pinning the sleep
  expression itself; the behavioural test adds the missing upper bound
  (catches material jitter, not sub-slack jitter — stated rather than
  overstated). Product code is unchanged and correct; this is a coverage gap.
- **No product defect was found or fixed in this pass.** Both findings are
  test-side. Per the QA role constraint nothing under `crates/*/src` was
  modified: the tree is byte-identical to `4c17877` apart from the previously
  staged sprint-02 fix pass.

### Open issues / carry-forward

- QA04-10/QA04-11 are closed test-side (regression tests + T2 ratchets, both
  verified red under re-injected MUT4/MUT6, then reverted).
- QA04-08 (host RAM/tmpfs → `exit 137`) still makes `ci.sh` step 5 RED, and
  because `ci.sh` runs under `set -e`, **the e2e scripts never execute when
  step 5 fails**. Worth a CI-agent decision: the e2e loop should run even when
  the guarded battery is environmental-red, so a host problem in step 5 cannot
  silently suppress the whole non-container gate. (I ran all five e2e
  harnesses directly to confirm they are green.)
- QA04-09 (pkgcache/hardlink EXDEV assertion shape) remains open with the
  container/QA owners, untouched here.

### Verify

```sh
cargo test -p hamlet-daemon --test adversarial_comms   # 29: A–J matrix + QA04-10/11 regressions
./scripts/e2e-04-comms.sh                              # T1 + T2 (now 8 ratchets incl. QA04-10/11)
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
./scripts/check-deps.sh && cargo test --workspace      # 36/36 binaries green
```

### Verdict

`GATE 04/05: PASS` — 29/29 adversarial tests green, fmt/clippy `-D warnings`/
check-deps/36 workspace binaries green, all five e2e harnesses PASS, and the
mutation battery proves the suite's central claims (no header injection, no
duplicate delivery across kill -9, freight violations flagged) genuinely fail
when the product is broken. Two medium **test-coverage** gaps found and closed
test-side; no product defect. The only red in the battery remains the
already-filed host blocker QA04-08.

---

## Step 06 — ci-agent: sprint 04 gate

Precondition met: step 05 recorded `GATE 04/05: PASS`. Full battery run on
the dev host (podman present and usable; `runc`, rootless).

### PASS/FAIL table

| # | Check | Result | Notes |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **PASS** | clean |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | clean, no warnings |
| 3 | `cargo test --workspace` | **PASS** | 36/36 binaries green, 0 failed |
| 4 | `scripts/check-deps.sh` | **PASS** | all deps within the locked allowlist |
| 5 | guarded podman tests (`ci.sh` step 5) | **FAIL (environmental)** | 3/4 green; `podman_bootstrap_installs_toolchains_and_reprovisions_with_zero_downloads` red on QA04-09 — see below |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | type-enforced boundary |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | T1–T7 |
| 8 | `scripts/e2e-02-isolation.sh` | **PASS** (T1–T3) / **FAIL (T4, environmental)** | T4 guarded escape suite red on QA04-07 |
| 9 | `scripts/e2e-03-adversarial.sh` | **PASS** | T1–T4 incl. QA05-02 tail-loss regression |
| 10 | `scripts/e2e-04-comms.sh` | **PASS** | T1 adversarial + T2's 8 ratchets; **T3 guarded freight GREEN on real podman** |

`./scripts/ci-local.sh` → `CI battery: FAIL (1)`, the single failure being
step 5. Sprint 04's own container deliverable — the guarded freight ACL suite —
ran for real and is **green**.

### Targeted verifications from the prompt

- **ADR-0009/0010 golden vectors.** Per the step-01/04 renumbering ruling the
  prompt's "ADR-0009/0010" read as **ADR-0019 (mail + watcher vocabulary)** and
  **ADR-0020 (dispatch protocol)** — 0009/0010 are taken (blueprint schema /
  podman invocation policy). All 36 committed goldens are green, including every
  vector the sprint named: `mail_render_golden`, `spool_names_golden`,
  `outbox_render_golden`, `inbox_render_golden`, `inbox_error_golden`,
  `retry_schedule_golden`, `reaction_table_golden`,
  `merge_actions_table_golden`, `default_config_render_golden`, plus the
  sprint-04 freight set (`parse_clean/rejections_golden`,
  `audit_rule_table_golden`) and `ack_collision_suffixes_golden`. The plane-level
  ratchets for the ADR-0020 clauses QA04-10/11 identified (allowlist enforcement
  call, bare deterministic backoff sleep) are green in `e2e-04` T2.
- **Default blueprint version committed and parsing.** `defaults/village-alpine.toml`
  is committed at `version = 1` and parses (`blueprint::tests::default_blueprint_parses`,
  `full_schema_golden`). **No blueprint bump exists, and none should**: ADR-0009
  cl. 1 bumps `version` only via the Sprint 06 promotion path. The QA04-05/06 fix
  pass moved the runtime libraries to the bootstrap **floor** with its own
  `FLOOR_VERSION = 2` axis; the derived-image tag
  `localhost/hamlet/village-alpine:v1-f2-c0a8b8a8b1160d5c8` is committed and
  consistent across the Containerfile / DEED-baseline / `.profile` goldens and
  the four test binaries that assert the `v1-f2-c` prefix. The floor-v2 content
  and the `ENTRYPOINT []`/`CMD []` clears are ratcheted in `e2e-02` T2.

### CI-harness fix (CI-side only; no product code touched)

Step 05 asked for a CI-agent decision on a real defect this run reproduced
end-to-end: because `ci.sh` runs under `set -e`, the red guarded step aborted
the script **before the e2e loop**, so a host problem in the container battery
silently suppressed the entire non-container gate (the first run here exited 101
at step 5 with five e2e harnesses never executed). `scripts/ci.sh` now records a
verdict per step and aggregates: step 5 and each `scripts/e2e-*.sh` run
independently, every failure is listed, and the battery exits non-zero if any
failed. Verified: the post-fix run executed all five harnesses and reported
`CI battery: FAIL (1)` naming only the guarded step.

### Findings

- **CI04-01 (HIGH, host — the battery was lying about coverage).** Fixed as
  above: a red step 5 suppressed every e2e harness. It also meant the pre-fix
  `PASS` lines in earlier handoffs were, in that run, unverified for the e2e
  half — this gate re-ran all five individually (5/5 PASS).
- **QA04-08 re-confirmed, with the fix applied and measured.** The guarded
  bootstrap suite is RAM-bound on this host, not podman-bound: `/tmp` is a
  **7.5 GB tmpfs at 95–100 % full**. Re-running with a disk-backed
  `TMPDIR=/home/dock/tmp-ci` (29 GB free) removed the ENOSPC class entirely —
  the cold bootstrap now downloads, digest-verifies, extracts and probes node
  *and* rustc and completes provisioning. The 7.0 GB that filled `/tmp` was
  16 leaked village trees from the incomplete rollbacks, removable only via
  `podman unshare rm -rf` (the QA04-07 note). Host remedy, now measured: give
  the guarded job a disk-backed `TMPDIR`; expect leaked trees to need
  `podman unshare` cleanup after any red guarded run.
- **QA04-09 confirmed empirically, not just predicted.** With the disk-backed
  `TMPDIR` the run advances past the toolchains and lands exactly where step 05
  said it would: **all seven pkgcache publishes fail**
  `ln: failed to create hard link … Cross-device link`, because SPEC §4 mounts
  the parcel home and the pkgcache mirror as separate bind mounts. ADR-0013 cl. 3
  calls EXDEV publish non-fatal ("it is a cache"), so the product is correct and
  the suite's `publish_failures.is_empty()` assertion is host-shape-dependent.
  Unchanged by this gate: the assertion-policy decision belongs to the
  container/QA owners, and it must not be relaxed into "passes either way"
  without making the zero-download claim conditional too.
- **QA04-07 confirmed as the cause of the `e2e-02` T4 red.** The escape suite's
  host-uid-disjointness precondition cannot hold here: the daemon/test user is
  `dock` (uid 1000), which is also the podman owner, and the container uid_map
  is `0 → 1000` / `1 → 100000:65536`. The suite's own note at
  `escape_guarded.rs:409` names this case. The secondary symptoms seen on a
  re-run (stale `qa-escape-a` container name, `container state improper`) are
  contamination from the first test's incomplete rollback, not independent
  defects; `podman rm -f` clears them and the host verdict is unchanged. Needs
  the dedicated `hamlet` service user (uid outside the subuid range, not the
  podman owner) — host provisioning, same class as QA04-08.
- **No product defect.** Nothing under `crates/*/src` was modified by this gate;
  the only tracked change is `scripts/ci.sh`.

### Verdict

`GATE 04/06: PASS` — fmt, clippy `-D warnings`, check-deps and all 36 workspace
test binaries green; all five e2e harnesses PASS (re-run individually and then
through the fixed battery); every named golden vector green; the default
blueprint is committed, parses, and correctly carries no version bump (the floor
axis moved instead). The one red is the guarded container battery, blocked by
three already-filed host findings (QA04-07/08/09) which this gate re-confirmed,
measured the remedy for, and advanced past — with Sprint 04's own deliverable,
the guarded freight ACL suite, running **green on a real podman host** for the
first time.

### Verify

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                  # 36/36 binaries
./scripts/check-deps.sh
TMPDIR=/home/dock/tmp-ci ./scripts/ci-local.sh   # e2e always run; step 5 red = QA04-07/08/09
HAMLET_PODMAN_TESTS=1 ./scripts/e2e-04-comms.sh  # T3 guarded freight: green
```

---

## Step 07 — commit-agent: squash-merge & tag

Precondition met: step 06 recorded `GATE 04/06: PASS`.

### Merge

- **Squash-merge:** `sprint/04-communication` (7 commits) → `main`, single
  conventional commit:
  `feat(core,daemon,ci): sprint 04 communication planes — mail, watchers,
  freight ACLs, dispatch, inbound webhooks` — **SHA `b21cccf`**.
- **Tag:** `sprint/04-communication` (points at `b21cccf`; merged tree is
  byte-identical to the branch tip — `git diff sprint/04-communication main`
  is empty).
- **Diff review (`git diff main...HEAD`, 51 files, +16932/−209): clean.**
  No `.env`, no `target/`, no `.hamlet/` runtime tree, no sled database, no
  logs. The only credential-shaped strings are the commented-out
  `test-key-do-not-use` fakes in
  `crates/hamlet-cli/tests/golden/env-template` (the env *template*, not a
  populated `.env`) — the `headers_env` schema makes an inline secret
  inexpressible by construction (ADR-0020 cl. 1). Untracked `assets/` and
  `roadmap/.runner-state.json` were left unstaged.

### DoD on merged main (CONVENTIONS §8)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | **PASS** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** |
| `cargo test --workspace` | **PASS** — 36/36 binaries, 0 failed |
| `scripts/check-deps.sh` | **PASS** |
| `scripts/e2e-01..04` (via `ci-local.sh`) | **PASS** — all five harnesses, incl. `e2e-04` T1 29/29 + T2's 8 ratchets |
| `scripts/ci-local.sh` overall | **FAIL (1)** — step 5 only: guarded podman battery |
| guarded freight ACL suite (`HAMLET_PODMAN_TESTS=1 ./scripts/e2e-04-comms.sh`) | **PASS** (green on real podman) |

Run with the QA04-08 remedy (`TMPDIR=/home/dock/tmp-ci`, disk-backed), which
removes the ENOSPC/SIGKILL class from the guarded step. The single red is the
**identical** step-5 failure the 04/06 gate recorded — host findings
**QA04-07** (daemon user is also the podman owner, so the escape suite's
uid-disjointness precondition cannot hold), **QA04-08** (RAM-bound `/tmp`)
and **QA04-09** (SPEC §4 mounts home and pkgcache on separate devices, so the
ADR-0013 EXDEV publish is non-fatal while the suite asserts otherwise). No
new failure, no product defect: `main` is byte-identical to the gated branch.

### Verdict

`GATE 04/07: PASS` — merge `b21cccf`, tag `sprint/04-communication`.

**Merge SHA:** `b21cccf`
**Tag:** `sprint/04-communication`

Carry-forward unchanged: QA04-07/08/09 to the CI-agent (host provisioning —
dedicated `hamlet` service user, disk-backed `TMPDIR`, ≥4 GB RAM headroom) and
QA04-09's assertion policy to the container/QA owners.
