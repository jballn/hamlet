# HANDOFF — Sprint 01 Host Topology

## Done

- **01-architect:** typed tree layout + SafeName sanitizer (branch
  `sprint/01-host-topology`).
  - `crates/hamlet-core/src/tree.rs` — `HamletTree { root }` with an
    accessor for EVERY normative SPEC §2 location: top-level files
    (`env_file`/`charter`/`config`), guild (register/blueprints/ledger,
    `commission(n)` + git/COMMISSION.md/INDENTURE.md), tavern
    (gossip/patrons, `patron(&SafeName)` + home.uid.bak/PROPRIETOR.md),
    courier (mail + lazy `courier_mail_recipient(&SafeName)`, freight,
    dispatch outbox/inbox), market (skills/pkgcache), keep (vault +
    `vault_artifact(&SafeName)`, archives, census, `parcel(uid)` +
    home/TITLE/DEED/PROPRIETOR, manor + home/MAYOR.md). Composition only
    via a private `join`; dir literals are private constants (each appears
    exactly once).
  - `HamletTree::ensure(root)` — the single sanctioned I/O point in
    hamlet-core (ADR-0003): idempotent, root-injected, creates all static
    dirs, creates/forces `.env` to 0600 (contents preserved). Per-UID and
    per-name dirs are lazy (daemon-owned).
  - `crates/hamlet-core/src/sanitize.rs` — CONVENTIONS §4 verbatim:
    regex `^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)?$`, length 1–64 chars,
    rejects `.`/`..`/leading `-`/control chars/separators/non-ASCII.
    `sanitize_name(&str) -> Result<SafeName, SanitizeError>`; `SafeName`
    is the only input accepted by name-taking tree accessors; serde
    Deserialize re-validates. Fixed check order: Empty → TooLong →
    PathSeparator → ControlChar → NonAscii → LeadingDash → DotName →
    InvalidShape.
  - Golden vectors committed in `sanitize.rs` tests: 19 accept + 35 reject
    (unicode, traversal, NUL, control chars, separators, overlength,
    shape) + single-path-segment and serde-revalidation tests.
  - ADRs: `docs/adr/0002-path-sanitizer.md`, `docs/adr/0003-tree-layout-api.md`
    (both Accepted; README index updated).
  - Deps added: `regex` (runtime, locked stack), `tempfile` + `serde_json`
    (dev). check-deps OK.
  - Verified: `cargo test -p hamlet-core` → 10 passed; `scripts/ci-local.sh`
    → `CI battery: PASS`.

- **02-systems-engineer:** `hamlet init` bootstrap + config/.env.
  - `crates/hamlet-core/src/config.rs` — full `config.json` schema (serde,
    `deny_unknown_fields`): `config_version: u32` (=1), `daemon_data_dir`
    (sled location; default `"sled"`, relative resolves against village root
    via `resolve_daemon_data_dir`), `podman_bin` (default `"podman"`),
    `base_image` (default `docker.io/library/alpine:3.20` until sprint 02
    ships the real Alpine+uutils image), `web_listen_addr` (default
    `127.0.0.1:8787`, validated as `SocketAddr`), `log_level` (lowercase
    enum, default `info`), `dispatch` placeholder (`enabled=false`,
    `timeout_secs=30`, validated 1..=3600). `parse`/`validate`/`render`
    (pretty + trailing newline); inline golden render vector in tests.
  - `.env` secrets: `Secret` newtype — **no `Serialize`, no `Display`,
    redacting `Debug`**, single greppable `expose()` accessor; typed
    `Secrets { model_endpoint, model_api_key, provider_keys: BTreeMap }`
    parsed from an injected reader via `dotenvy::from_read_iter` (NOT
    `from_read`, which mutates process env → purity-seam breach).
    `HAMLET_MODEL_ENDPOINT` / `HAMLET_MODEL_API_KEY` /
    `HAMLET_API_KEY_<PROVIDER>`; empty values = unset (never shadow a real
    duplicate); unknown vars ignored. `ENV_TEMPLATE` (all-comment,
    `test-key-do-not-use` fakes, `example.invalid` endpoints) is written by
    init at 0600; tests assert the template parses to empty `Secrets` and
    that `Debug` dumps redact every value.
  - `crates/hamlet-core/src/charter.rs` — coordination stub for step 3:
    `CHARTER_VERSION = 1`, `Charter { charter_version }`, `Charter::empty()`,
    `render()`; golden `"{\n  \"charter_version\": 1\n}\n"`. Tolerant parse
    (no `deny_unknown_fields`) so step 3 / ADR-0004 can add content groups
    additively.
  - `tree.rs` additions: `VILLAGE_DIR_NAME` (`.hamlet` literal lives once),
    `HamletTree::in_working_dir(dir)` (root derivation), `static_dirs()`
    (root + ENSURE_DIRS, for mode normalization). Lazy dirs excluded.
  - `hamlet-cli` is now lib+bin: `init::run(<dir>)` → `InitReport`
    (tree/config/charter/env × Created|Existed). Writes are atomic
    (same-dir temp → write → fsync → set mode → rename → dir fsync,
    CONVENTIONS §3) and **never clobber**: existing non-empty artifacts are
    untouched; an existing *empty* `.env` (bare `ensure` output) receives the
    template. Canonical modes enforced deterministically under any umask:
    static dirs 0755, JSON artifacts 0644, `.env` 0600. `main.rs` hand-rolls
    `init <dir>` / `help` / `--version` (clap stays sprint 08); usage
    errors exit 2.
  - Tests: core 25 (config goldens, validation rejects, secret redaction,
    malformed-.env). CLI: 3 unit (create/idempotent report, clobber-guard on
    edited artifacts, partial-tree completion) + 6 golden integration
    (`tests/init_golden.rs`, committed fixtures under `tests/golden/`:
    full tree listing with modes, config.json, charter.json, env-template;
    idempotency + byte-stability; explicit 0600; end-to-end via
    `CARGO_BIN_EXE_hamlet` incl. exit codes).
  - Deps: `serde_json` moved to core runtime, `dotenvy` added (both locked
    stack). check-deps OK. `cargo test --workspace` green;
    `scripts/ci-local.sh` → `CI battery: PASS`; manual
    `hamlet init /tmp/hamlet-test` inspected (tree, goldens, re-run no-op).

- **03-systems-engineer:** charter state model + UID slot allocation
  (ADR-0004).
  - `crates/hamlet-core/src/charter.rs` — full v1 `Charter`: all six
    normative SPEC §11.1 groups as `BTreeMap`/`BTreeSet` with
    `deny_unknown_fields`: `slots` (`SlotTable` = uid → `SlotRecord`),
    `agents` (`SafeName` → `AgentRef { slot }`, sprint 03 placeholder),
    `commissions` (u64 → `CommissionStatus::Open|Closed`, sprint 06
    placeholder), `parcels` (uid → `ParcelLease { identity, stasis }`),
    `decrees` (u64 → `DecreeStatus::Queued|Delivered|Failed`, sprint 05
    placeholder), `webhooks` (`BTreeSet<SafeName>` config refs). Empty
    groups skip serialization → `Charter::empty()` still renders the init
    golden `{"charter_version": 1}` (CLI goldens untouched, in sync).
  - Canonical JSON: `render()` goes through `serde_json::to_value`
    (objects are BTreeMaps → keys sorted at *every* level, not just
    declaration order) + pretty + trailing newline; deterministic →
    byte-identical for equal state. Golden vectors committed for both
    empty and fully populated charters.
  - `load(path)` / `from_bytes`: parse → version gate (0 or > current =
    `UnsupportedVersion`) → additive-only `migrate()` skeleton (explicit
    per-version steps, none yet) → `validate()` cross-group invariants
    (parcel↔slot and agent↔slot identity match, slot commission refs
    exist, uid range policy). Corruption = `CharterError::Corrupt` with
    line/column/**byte offset** (derived from serde position over raw
    bytes; clamps to EOF). No silent recovery anywhere.
  - `save_atomic(path)`: same-dir temp `.<name>.tmp` → write → fsync →
    chmod 0644 → rename → dir fsync; best-effort temp cleanup on failure;
    never in place. Second (and only other) sanctioned core I/O point,
    extending ADR-0003's `ensure()` precedent — recorded in ADR-0004;
    `lib.rs` purity note updated. CLI `init` now calls
    `Charter::empty().save_atomic(...)` so exactly one atomic-write
    implementation exists for charter.json.
  - `crates/hamlet-core/src/slots.rs` — `MAYOR_UID = 1000`,
    `FIRST_AGENT_UID = 1001`, `LAST_AGENT_UID = 2000`,
    `AGENT_SLOT_CAPACITY = 1000`. `SlotRecord { identity: SafeName,
    status: SlotStatus::Active|Hibernated|Crashed, commission:
    Option<u64> }`. `reserve_mayor` (idempotent; typed `MayorConflict` on
    a different identity), `allocate` (sequential lowest-free scan;
    idempotent per identity — permanence; existing records never
    mutated), `hibernate` / `mark_crashed` / `reactivate` transitions,
    `next_free_uid`. **No removal API exists** — permanence is structural;
    hibernated slots keep their mapping (SPEC §10) and are never handed to
    a new identity. Exhaustion beyond 2000 → `SlotError::Exhausted`
    (typed hard error, never a wrap). `Charter::reserve_mayor` /
    `Charter::allocate_slot` delegate over `self.slots`.
  - Tests (core now 42): kill/restart resumability — save, `drop()`
    without flush (`kill -9` equivalent), reload, assert equal state AND
    byte-identical re-serialization vs committed bytes; torn-write —
    truncated temp beside a committed charter leaves the previous charter
    intact, `load` ignores the temp, next `save_atomic` succeeds and
    removes the stale temp; exhaustion through the Charter API leaves no
    partial state; corruption byte-offset goldens; 6 invariant-violation
    rejects; slot sequence/idempotency/hibernation-no-reuse goldens.
  - ADR-0004 written (`docs/adr/0004-charter-schema.md`, Accepted; README
    index updated): v1 schema, canonical-render rule, atomic protocol,
    fail-hard load, additive-only migration policy, UID permanence.
  - Verified: `cargo test -p hamlet-core` → 42 passed;
    `cargo test --workspace` + `scripts/ci-local.sh` → `CI battery: PASS`.

- **04-systems-engineer:** sled telemetry store + archives export
  (ADR-0005).
  - `crates/hamlet-daemon/src/telemetry.rs` — `Telemetry` over sled at the
    config-resolved daemon data dir. `open(data_dir)` / `open_for_village`
    (resolves `Config::daemon_data_dir`, **hard-rejects any location inside
    the `.hamlet/` tree** → `TelemetryError::InsideVillageTree`). Store
    never reads the wall clock — every `at_unix_ms` is caller-supplied.
  - Key layout (ADR-0005): 5 schema-suffixed trees — `tokens.v1`,
    `costs.v1` (cumulative per session), `latency.v1`, `traces.v1`
    (append-only), `counters.v1` (per-agent seq allocators, `domain u8`
    prefix). Keys fixed-width big-endian `commission u64 | uid u32 |
    session u64` (20 B; agent scope = 12-B prefix; append-only + `seq
    u64`) → byte sort = commission→uid→session→seq; queries/prunes are
    prefix scans. Values: canonical JSON records (`deny_unknown_fields`)
    except raw `u64` BE counters. Mutators are CAS loops
    (`fetch_and_update`) with a typed fail-hard pre-check (corrupt stored
    record → `TelemetryError::Corrupt`, never silently re-zeroed).
  - API: `add_tokens` / `add_cost_micro_usd` (micro-USD integer math),
    `record_latency_spike` / `trace_event` (return monotonic 1-based
    seqs), queries `session_tokens`, `session_cost_micro_usd`,
    `agent_totals`, `commission_totals` (totals + per-uid `BTreeMap`),
    `recent_traces(agent, window)`; `export_commission_summary` →
    canonical JSON `keep/archives/commission-<n>-telemetry.json` via the
    typed tree accessor, atomic temp→fsync→chmod 0644→rename→dir-fsync
    (ADR-0004 protocol; Sprint 06 wires it at commission close);
    `prune_commission` (retention: archive = durable record); `flush()`
    is the durability checkpoint.
  - **Relocated step 2's provisional default** (ratified path per step 2's
    binding note): `daemon_data_dir` default `sled` → `.hamlet-data/sled`,
    and `resolve_daemon_data_dir` now resolves relative paths against the
    *working directory containing* the village root, not the root itself —
    telemetry is out-of-band, never inside the tree. `config.json` golden
    (core test + `crates/hamlet-cli/tests/golden/config.json`) updated in
    the same commit; additive change within `config_version` 1.
  - Tests (daemon 10): key-encoding golden vectors (fixed-width BE,
    ordering, prefix relation), write/read round-trips (accumulation),
    append-only seqs + trace window (recent-N, oldest-first), summary math
    (per-agent/per-commission, unknown commission = empty not error),
    **export golden file** (`tests/golden/commission-7-telemetry.json`,
    byte-compared; deterministic re-export; no temp leftovers; typed
    round-trip; regeneration only via deliberate
    `HAMLET_UPDATE_GOLDEN=1`), prune-after-export (other commission
    untouched, archive survives, seqs restart), **boundary test** (tree
    allowlist = `TELEMETRY_TREES` + empty sled default tree, fixed-width
    keys only, charter tokens `slot`/`status`/`decree`/`parcel`/… banned
    from all keys/values, charter JSON rejected by record decoders),
    inside-tree data dir rejected, corrupt value fail-hard, and
    **kill/restart resumability**: a subprocess writes → `flush()`
    checkpoint → writes unflushed tail → `exit(9)` (no destructors,
    `kill -9` semantics); parent reopens — db recovers, checkpointed data
    intact, store writable, clean close+reopen byte-stable.
  - Deps added to hamlet-daemon: `hamlet-core`, `sled`, `serde`,
    `serde_json` (all locked stack); dev: `tempfile`. check-deps OK.
    `.gitignore` += `.hamlet-data/`. ADR-0005 written (Accepted; README
    index updated).
  - Verified: `cargo test -p hamlet-daemon` → 10 passed;
    `cargo test --workspace` green; `scripts/ci-local.sh` →
    `CI battery: PASS`.

- **05-qa-engineer:** adversarial suite — topology & state (committed tests
  + e2e harnesses; QA does not fix product code).
  - `crates/hamlet-core/tests/adversarial_sanitize.rs` (7 tests, ALL PASS):
    hostile-class battery (traversal `../`, encoded `..%2f`, absolute
    `/`+`\`+`C:\`, NUL lead/mid/tail, C1 controls, ESC sequences, unicode
    homoglyphs — Cyrillic/fullwidth/dotless-ı, DIVISION+FRACTION SLASH and
    FULLWIDTH SOLIDUS `/` lookalikes, RTL override, ZWSP/BOM, ligatures —
    65/64/100 000-char lengths, leading dash); accept round-trip contract
    (as_str identity, re-sanitize stability, serde string round-trip,
    single-path-segment join invariant); exhaustive length-≤3 sweep over a
    15-char hostile alphabet (~3 000 candidates: every accept round-trips
    and cannot escape a join, every reject is a typed `SanitizeError` with
    non-empty Display); serde as trust boundary (hostile names in JSON
    rejected, incl. nested `SlotRecord.identity`); `boundary_proof` module
    (runtime half of item 2: immutable views only, no interior mutability,
    value-ordered BTree semantics).
  - `crates/hamlet-core/tests/adversarial_state.rs` (13 tests: 11 pass,
    **2 fail = findings F01-01, F01-04**): REAL `kill -9` mid-save via
    SIGKILLed subprocess (25+ committed versions in flight) → charter is
    always a complete contiguous prefix, byte-identical canonical re-render,
    stale temp ignored by `load` and consumed by next save; hostile temp
    states beside a committed charter (torn half, garbage, complete-stale,
    DIRECTORY squatting `.charter.json.tmp` → typed Io error, committed
    charter survives); failed save (read-only dir) leaves committed bytes +
    no temp litter; corrupt-JSON byte offsets accurate for interior,
    multi-line, trailing-garbage, truncation (clamps to EOF), invalid
    UTF-8, unsafe identity strings, unknown enum/field variants; version
    gate vs corruption precedence; slot exhaustion at the 1001st identity
    through the Charter API + persistence (still exhausted after reload;
    existing identity still resolves when full); hibernated/crashed slots
    never reused (new identities skip parked uids; same identity maps back;
    allocate never silently flips status); double-allocation idempotent
    (no duplicate, commission ref not overwritten, mayor conflict typed);
    crafted out-of-range uids (0/1/999/2001/MAX) rejected at load;
    full-cycle resumability (init → allocate/hibernate → save → fresh
    load-from-disk-only → identical state + byte-identical render +
    coherent continued allocation); time-independence (same ops → same
    bytes, sorted keys at every level).
  - `crates/hamlet-cli/tests/adversarial_init.rs` (8 tests: 4 pass,
    **4 fail = findings F01-02/03/05/06**): re-init byte+mode no-op
    snapshot; `.env` EXACTLY 0600 under `umask 000` subprocess (and
    re-tightened after tampering, content preserved); `.hamlet` squatted
    by a regular file → typed refusal, squatter untouched; full cycle
    through the REAL binary (`CARGO_BIN_EXE_hamlet` init → charter load →
    allocate → save → subprocess restart child loads from disk only,
    allocates, saves, `exit(9)` → parent reload → identical state +
    byte-identical render).
  - `scripts/e2e-01-safename-compile-fail.sh` (PASS): item 2 compile-FAIL
    proof — 7 `rustc` probes against the built rlib: raw `&str`/`String`
    into `vault_artifact`/`courier_mail_recipient`/`patron`,
    `SafeName(..)` tuple construction, `SafeName::from`, private-field
    mutation — ALL must fail to compile; control probe must compile.
    **Recorded choice: script-based rustc probes, NOT `trybuild`** —
    trybuild is not on the locked dev-dep allowlist (check-deps.sh;
    adding it needs an ADR); the documented type-level proof lives in
    `adversarial_sanitize.rs::boundary_proof` + ADR-0002.
  - `scripts/e2e-01-topology-state.sh` (PASS): binary-level init e2e —
    normative dir set, canonical modes under hostile umask, empty-charter
    golden bytes, re-init sha256+mode stability, no temp litter, .env
    tamper re-tighten, `.hamlet`-as-file refusal (exit 1), read-only dir
    refusal with typed stderr, usage exits 2, secrets hygiene (template
    fakes only, nothing secret-looking outside `.env`).
  - No product code touched; no new dependencies (check-deps OK);
    `cargo fmt --all --check` + `cargo clippy --workspace --all-targets
    -- -D warnings` green.

- **06-fix-pass:** all six QA findings resolved in PRODUCT code (tests
  untouched — the adversarial suite is the contract; no new dependencies).
  - **F01-01** (ADR-0006): `charter.rs` `save_atomic` writes to a UNIQUE
    temp per attempt (`.<name>.<pid>.<serial>.tmp` — pid + process-wide
    atomic counter; concurrent savers never share a temp) and sweeps the
    legacy deterministic temp (`Charter::temp_path`, public API unchanged)
    before writing: stale files consumed by the next save, un-sweepable
    squats (directory) hard-fail BEFORE the committed charter is touched.
    Same unique-temp protocol in `init.rs` `atomic_write_inner`.
  - **F01-02/06** (ADR-0007): `tree.rs` `ensure` classifies `.env` via
    `symlink_metadata` — regular → chmod 0600 (content preserved); absent
    → `create_new` (`O_EXCL`, never follows a planted link); symlink/dir/
    other → typed refusal. `init.rs` `classify_artifact` applies the same
    lstat rule to all three artifacts (`has_content` removed).
  - **F01-03** (ADR-0007): `init.rs` `refuse_foreign_content` — a present
    village root that is not a real directory, or that holds entries
    outside the normative top-level allowlist (derived from
    `static_dirs()` + artifact names, never hand-maintained), is refused
    typed BEFORE anything is created inside; foreign content untouched.
  - **F01-04** (ADR-0008): `Charter::validate` rejects one identity bound
    to multiple slots (`Invariant`, uid-sorted first-violation order).
  - **F01-05** (ADR-0007): `normalize_file_mode` re-normalizes existing
    `config.json`/`charter.json` to 0644 on every init run (content
    untouched).
  - ADRs 0006/0007/0008 written (Accepted; README index updated);
    ADR-0003/0004 status lines amended per the supersede rule.
  - Verified: `cargo test --workspace` → 89 passed / 0 failed (adversarial
    28/28 green); `concurrent_savers_never_tear_or_fail` stress: 12 runs ×
    300 raced rounds → 0 failures (was 10/10 failing pre-fix); both e2e
    harnesses PASS; `scripts/ci-local.sh` → `CI battery: PASS`;
    fmt/clippy `-D warnings`/check-deps green.

- **05-qa re-verification (independent, post-fix @ `0c858ac`):** fixes
  accepted — GATE 01/05 flips to **PASS**. QA does not take the fixer's
  word; everything below was re-run and re-probed.
  - **Integrity:** `git diff 6430f28..HEAD -- crates/*/tests scripts` →
    **0 files changed** — the committed suite (the contract) was not
    edited, weakened, or skipped; product-only fixes.
  - **Battery:** fmt ✅ clippy `-D warnings` ✅ check-deps ✅ both e2e
    harnesses ✅ golden tests no drift (init_golden 6/6) ✅
    `cargo test --workspace --no-fail-fast` → **95 passed / 0 failed**
    (was 83/6 at `6430f28`; the six finding tests now pass for the
    documented EXPECTED behavior).
  - **Stress (probabilistic fixes):** `concurrent_savers_never_tear_or_fail`
    10/10 invocations × 300 raced rounds green (pre-fix: failed round 0–1,
    10/10 red); SIGKILL-mid-save 5/5; binary full-cycle `exit(9)` restart
    5/5; new concurrent-init race 5/5 × 50 rounds.
  - **Bypass probes against the NEW boundary — all refused/clean:**
    symlinked `charter.json`/`config.json` (external target byte- and
    mode-untouched); village root symlinked to an outside dir (nothing
    created inside); dangling root symlink (target never materialized);
    FIFO at `.env`; crash-litter temps (ADR-0006 unique name AND legacy
    name) → refused as foreign, litter preserved as crash evidence,
    charter still loadable, init recovers after operator cleanup
    (ADR-0007 documented consequence — intentional, now pinned).
  - **ADR-0008 edges:** duplicate identity spanning mayor+agent slots
    rejected; three-way duplicate error names identity AND offending uid;
    a full 1000-slot table of unique identities still validates.
  - **Six new committed regression pins** (so a refactor can't silently
    reopen the fixed holes): `adversarial_init.rs` +=
    `symlinked_json_artifacts_are_refused_target_untouched`,
    `village_root_symlink_is_refused_outside_untouched`,
    `crash_litter_temps_make_init_refuse_loudly`, `env_fifo_is_refused`,
    `concurrent_inits_never_tear_artifacts`; `adversarial_state.rs` +=
    `duplicate_identity_including_mayor_is_rejected`. Suite totals now:
    sanitize 7, state 14, init 13.

## Findings — Sprint 01 QA (prompt 05/05)

Each finding has a committed failing test that pins the contract; fix the
product code, not the test. Six tests failed by design (gate was RED):
`cargo test --workspace --no-fail-fast`.

**All six findings RESOLVED by the fix pass (Done → `06-fix-pass`;
ADR-0006/0007/0008). The adversarial suite is fully green — the entries
below are preserved as the historical contract record; every listed test
now passes for the documented EXPECTED behavior. Product code was fixed;
no test was edited.**

> **Re-verified (QA 05, at commit `6430f28`):** all six findings reproduce
> on demand. F01-01 failed **10/10** consecutive runs (first observed race
> was `Io { NotFound }` in round 1 — treat as a hard RED, not a flake).
> Suite status exactly as pinned: `adversarial_sanitize` 7/7 green,
> `adversarial_state` 11/13 (F01-01, F01-04 red), `adversarial_init` 4/8
> (F01-02/03/05/06 red); every assertion message matches its finding; all
> cited file:line locations re-checked against product source; both e2e
> harnesses PASS (compile-fail boundary + topology-state); fmt/clippy/
> check-deps green; QA commit touches tests/scripts/HANDOFF only — no
> product code.

### F01-01 (CRITICAL) — concurrent `save_atomic` can publish a TORN charter
- **Where:** `crates/hamlet-core/src/charter.rs:253` (`temp_path` —
  deterministic `.charter.json.tmp`) + `:312` (`save_atomic`); same pattern
  in `crates/hamlet-cli/src/init.rs:254` (`atomic_write_inner`) for
  concurrent `hamlet init`.
- **Repro:** `cargo test -p hamlet-core --test adversarial_state \
  concurrent_savers_never_tear_or_fail` — two threads save different
  charters to one path; fails within a few rounds (spurious Io ~10 % of
  rounds; torn publish observed ~1/400 rounds).
- **Expected (CONVENTIONS §3 rule 2; SPEC §11.1):** every write atomic —
  both savers succeed or fail cleanly; `charter.json` always loads as one
  complete committed version.
- **Actual:** both savers share ONE temp file. Observed: (a)
  `Io { NotFound }` when the peer renames our temp away; (b) cleanup
  `remove_file` racing the peer; (c) worst case saver A renames the temp
  while saver B is mid-write → `charter.json` committed as
  `Corrupt { trailing characters }` — the master state map is destroyed
  with no prior good copy semantics violated in spirit (kill -9 alone is
  safe; concurrency is not).
- **Fix direction (for the fixer):** unique temp suffix (pid+counter) or an
  flock around the write-rename; do NOT widen the API.
- **RESOLVED (ADR-0006):** unique temp per attempt (`.<name>.<pid>.<serial>
  .tmp`) + legacy-temp sweep in `save_atomic`; same protocol in
  `atomic_write_inner`. API unchanged. Stress: 0 failures in 12 × 300
  raced rounds.

### F01-02 (CRITICAL, security) — init/`ensure` follows a symlinked `.env`
- **Where:** `crates/hamlet-core/src/tree.rs:132-143` (`ensure`:
  `env.exists()` / `File::create` / `set_mode_0600`→`set_permissions`, all
  symlink-following) + `crates/hamlet-cli/src/init.rs:148,172`
  (`has_content` follows too).
- **Repro:** `cargo test -p hamlet-cli --test adversarial_init \
  init_must_not_follow_symlinked_env_outside_tree` — plant
  `.hamlet/.env → <outside-file>`; run `hamlet init`.
- **Expected (SPEC §2 name-safety; §11.4; CONVENTIONS §6):** init refuses
  or atomically replaces the link; it must NEVER touch the link target.
- **Actual:** (1) existing external file gets `chmod 0600` through the link
  (observed 0644→0600 — mode-oracle/DoS primitive against any file the
  user can write); (2) dangling link: `File::create` CREATES the external
  target (0600) outside the village tree.
- **Fix direction:** `symlink_metadata` check on `.env` (and ideally every
  artifact path) — refuse or unlink-and-replace; `File::create` →
  `OpenOptions` + `custom_flags(O_NOFOLLOW)` where available.
- **RESOLVED (ADR-0007):** `ensure` lstat-classifies `.env` (refusal on
  non-regular; `create_new`/O_EXCL creation); init lstat-classifies all
  three artifacts. Chosen semantics: typed REFUSAL (fail-hard), never
  replace-in-place.

### F01-03 (MAJOR) — init adopts a foreign non-empty `.hamlet/` dir
- **Where:** `crates/hamlet-cli/src/init.rs:140-200` (`run` only stats its
  three artifacts; no foreign-content detection).
- **Repro:** `cargo test -p hamlet-cli --test adversarial_init \
  init_refuses_foreign_content_inside_dot_hamlet` — `.hamlet/` containing
  `NOT_MINE.txt` + `some-other-app/` → init succeeds (tree=Existed) and
  scatters hamlet artifacts among the foreign content.
- **Expected (QA contract item 5; SPEC §2 normative topology):** typed
  refusal when `.hamlet/` holds entries outside the normative layout
  (partial-tree completion for *empty* normative dirs may stay).
- **Actual:** silent adoption.
- **RESOLVED (ADR-0007):** `refuse_foreign_content` — normative top-level
  allowlist derived from the typed tree API; typed refusal before anything
  is created inside; foreign entries untouched.

### F01-04 (MINOR) — `validate()` accepts one identity holding TWO slots
- **Where:** `crates/hamlet-core/src/charter.rs:379` (`validate` checks
  parcel↔slot / agent↔slot / commission refs / uid range, but not slot
  identity uniqueness); ambiguity bites `slots.rs:152` (`allocate`'s
  find-by-identity returns the lowest uid).
- **Repro:** `cargo test -p hamlet-core --test adversarial_state \
  duplicate_identity_across_slots_must_violate_invariant` — hand-edited
  charter binding `ada` to 1001 AND 1002 loads `Ok`.
- **Expected (SPEC §4 "permanently per agent identity"; ADR-0004
  fail-hard):** `CharterError::Invariant`.
- **Actual:** accepted; hibernation/awakening of "the" slot is undefined.
- **RESOLVED (ADR-0008):** identity-uniqueness invariant in `validate()` —
  `Invariant` error names the identity and offending uid; runs on every
  load. No schema change (`charter_version` stays 1).

### F01-05 (MINOR) — re-init does not re-normalize JSON artifact modes
- **Where:** `crates/hamlet-cli/src/init.rs:150-175` (no chmod on the
  `*_existed` branches; contrast `normalize_dir_modes` for dirs and the
  unconditional `.env` tighten).
- **Repro:** `cargo test -p hamlet-cli --test adversarial_init \
  reinit_renormalizes_json_artifact_modes` — chmod 0666 config.json /
  0600 charter.json, re-init → modes persist.
- **Expected:** step-02's binding note: "Init enforces canonical modes on
  every run (static dirs 0755, artifacts 0644/0600)".
- **Actual:** only dirs and `.env` are enforced; artifact-mode claim is
  false for existing files.
- **RESOLVED (ADR-0007):** `normalize_file_mode` re-normalizes existing
  JSON artifacts to 0644 on every run; content never touched.

### F01-06 (MINOR) — `.env` occupied by a DIRECTORY is silently accepted
- **Where:** `crates/hamlet-cli/src/init.rs:207` (`has_content`: dir
  metadata len 4096 > 0 → "exists with content") + `tree.rs:132` (`ensure`
  chmods the directory to 0600).
- **Repro:** `cargo test -p hamlet-cli --test adversarial_init \
  env_path_occupied_by_directory_is_refused` → report `env: Existed`,
  village has no usable secrets file, no diagnostic.
- **Expected:** typed refusal (`.env` must be a regular file).
- **RESOLVED (ADR-0007):** non-regular `.env` (dir/symlink/other) refused
  typed in both `classify_artifact` (init) and `ensure` (core).

### Forward note (non-gating, sprint 06/07)
- A VALID `SafeName` can still name a pre-planted symlink inside a lazy
  dir (`keep/vault/<name>`, `tavern/patrons/PROPRIETOR<name>`). SafeName
  guarantees a single safe *segment*, not symlink-freedom. Promotion and
  hibernation copy code must `lstat`/`O_NOFOLLOW` destinations before
  writing (SPEC §4 isolation invariant: "cannot escape via path traversal
  in promoted filenames").

## Decisions

- **QA 05 (recorded choice, item 2):** compile-fail enforcement via
  `scripts/e2e-01-safename-compile-fail.sh` (raw `rustc` probes) +
  documented type-level proof in `adversarial_sanitize.rs::boundary_proof`
  — `trybuild` rejected (not on the locked dev-dep allowlist; would need an
  ADR + check-deps change for test-only convenience).
- **ADR-0002**: sanitizer shape/error taxonomy/check order; SafeName as
  type-level boundary; multi-dot names rejected (encode as `v1-2-3.tar.gz`).
- **ADR-0003**: accessor-only path policy; `ensure()` is the one recorded
  purity-seam exception (idempotent, root-injected); eager static dirs vs
  lazy identity-scoped dirs.
- Commission dirs stay index-typed (`commission(n: u64)`, SPEC §2
  `commission<n>/`); no slug layout invented — if slugs ever land, the
  slug segment must be a `SafeName` per ADR-0002.
- **Step 2 (config/init), binding until superseded by an ADR:**
  - `config.json`: `deny_unknown_fields` (typos fail loudly); version
    gate accepts only `1..=CONFIG_VERSION`; defaults as listed above.
  - **`daemon_data_dir` default `"sled"` → `.hamlet/sled`** (relative
    resolves against the village root): telemetry lives with the village,
    never committed (CONVENTIONS §7), never orchestration state
    (file-as-truth intact — charter.json stays the only truth). **Step 4
    (ADR-0005) must ratify or relocate this**; a move is an additive
    config change (defaults may change within `config_version` 1 while
    version is 0.0.0).
  - Init enforces canonical modes on every run (static dirs 0755,
    artifacts 0644/0600) so the golden listing is umask-independent;
    `.env` remains the only 0600 file — the village tree holds no other
    secret material.
  - `charter_version = 1` is the agreed skeleton version for init;
    ADR-0004 (step 3) owns the v1 schema from here.
- **ADR-0004**: charter v1 schema (six normative groups + placeholders),
  canonical JSON via `to_value` (sorted keys at every level), atomic
  temp→fsync→rename as the second sanctioned core I/O point, fail-hard
  load with byte offsets, additive-only per-version migration steps,
  structural slot permanence (no removal API; exhaustion is typed).

- **ADR-0005**: sled db lives OUTSIDE the village tree (default
  `<working-dir>/.hamlet-data/sled`; `open_for_village` enforces) —
  supersedes step 2's provisional `.hamlet/sled` default as that note
  anticipated; 5-tree namespace + fixed-width BE key layout; archives
  export = durable record, sled prunable; `flush()` checkpoint durability
  contract; boundary (telemetry-only) enforced by allowlist test.
- **ADR-0006** (fix pass): concurrency-safe atomic writes — unique temp
  per attempt (pid+serial), legacy-temp sweep before each save, rename
  stays the only commit point. Supersedes ADR-0004 clause 3 temp naming.
- **ADR-0007** (fix pass): bootstrap FS boundary — lstat classification of
  every artifact (never follow/adopt non-regular paths; O_EXCL creation),
  foreign-content refusal against a tree-API-derived allowlist, canonical
  artifact modes re-normalized on every init run. Amends ADR-0003.
- **ADR-0008** (fix pass): slot identity uniqueness is a load-time
  `Invariant` (one identity, one permanent slot — SPEC §4). Extends
  ADR-0004 clause 4.

## Open issues

- **GATE 01/05: PASS** — all six findings (F01-01 … F01-06, two CRITICAL)
  resolved by fixing PRODUCT code; the committed adversarial tests were
  never edited (`git diff 6430f28..HEAD -- crates/*/tests scripts` = 0
  files) and now pass. **Independently re-verified by QA** (Done →
  `05-qa re-verification`): workspace 95 tests / 0 failed incl. six new
  regression pins; concurrency stress 10×300 rounds green; boundary
  bypass probes (symlinked artifacts/root, FIFO, crash litter, concurrent
  init) all refused clean. Both e2e harnesses PASS;
  `scripts/ci-local.sh` → `CI battery: PASS`. Ready for merge review
  (step 07).
- Sprint 01 steps 1–4 landed. Sprint 06 wires
  `export_commission_summary` into commission close (call export BEFORE
  `prune_commission`); Sprint 02+ daemon code consumes `Telemetry::open_for_village`
  with `Config::resolve_daemon_data_dir` semantics (working-dir base).
- Sprint 06/07 promotion + hibernation copies must lstat/`O_NOFOLLOW`
  destination paths (see Forward note in Findings).

## Verify

```sh
cargo test -p hamlet-core        # golden vectors + tree tests
cargo test -p hamlet-cli         # init unit + golden-file integration
cargo test -p hamlet-daemon      # telemetry store: 10 tests incl. kill/restart
cargo run -p hamlet-cli -- init /tmp/hamlet-test   # manual smoke
./scripts/ci-local.sh            # full battery

# QA 05 adversarial suite (GREEN — F01-01…F01-06 fixed by the fix pass):
cargo test -p hamlet-core --test adversarial_sanitize --test adversarial_state
cargo test -p hamlet-cli --test adversarial_init
./scripts/e2e-01-safename-compile-fail.sh   # type-boundary compile-FAIL probes
./scripts/e2e-01-topology-state.sh          # binary-level init/topology e2e

# F01-01 regression stress (concurrency race, 300 rounds per invocation):
cargo test -p hamlet-core --test adversarial_state concurrent_savers_never_tear_or_fail
```

---

## GATE 01/06 — CI verification battery (ci-agent)

Precondition checked: `GATE 01/05: PASS` present (QA re-verification @
`0c858ac`, committed at `996ea82`). Battery run at HEAD `996ea82`, working
tree clean (no product-code edits; this gate touched only HANDOFF.md).

| # | Check | Result | Notes |
|---|-------|--------|-------|
| 1 | `cargo fmt --all --check` | **PASS** | via `scripts/ci-local.sh` [1/5] |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** | [2/5], zero warnings |
| 3 | `cargo test --workspace` | **PASS** | [3/5] — **95 passed / 0 failed** (core 42+7 sanitize+14 state, cli 3+6 golden+13 init, daemon 10); matches QA 05 re-verification count |
| 4 | `scripts/check-deps.sh` | **PASS** | [4/5] — "all dependencies within the locked allowlist" |
| 5 | Guarded podman tests | **SKIP (by design)** | `podman` not on PATH on this host → ci.sh records `SKIP: podman not available`; no `#[ignore]`d podman tests exist yet (exit 0 contract, absence must not fail the battery) |
| 6 | `scripts/e2e-01-safename-compile-fail.sh` | **PASS** | 7/7 hostile probes fail to compile, control probe compiles — SafeName boundary type-enforced |
| 7 | `scripts/e2e-01-topology-state.sh` | **PASS** | T1–T7 all ok (normative dirs, modes under umask 000, golden charter bytes, re-init no-op, .env tamper re-tighten, squatter/read-only refusal, exit codes, secrets hygiene) |

Overall: `scripts/ci-local.sh` → `CI battery: PASS`.

### ADR 0002–0005 exist and match the code they ratify

- **ADR-0002** (path sanitizer) ↔ `hamlet-core/src/sanitize.rs`: shape
  regex byte-identical to the ADR (`^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)?$`,
  sanitize.rs:25), `MAX_NAME_LEN = 64` (:28), documented check order,
  `SafeName` opaque newtype, serde re-validation, golden vectors committed.
- **ADR-0003** (tree layout API) ↔ `hamlet-core/src/tree.rs`:
  accessor-only `HamletTree` with private `join`; `ensure(root)` exists
  (:140) as the sanctioned I/O point; `.env` 0600 clause verified live by
  e2e T1/T3/T4 (amendment by ADR-0007 noted in the ADR status line).
- **ADR-0004** (charter schema) ↔ `hamlet-core/src/charter.rs`:
  `charter_version: u32` mandatory, `deny_unknown_fields` on all groups,
  canonical sorted-key render, fail-hard load with byte offsets,
  additive-only `migrate()`; temp-naming clause correctly marked superseded
  by ADR-0006 — code uses the unique `.<name>.<pid>.<serial>.tmp` protocol
  (charter.rs:17–18 docs + implementation).
- **ADR-0005** (sled key layout) ↔ `hamlet-daemon/src/telemetry.rs` +
  `hamlet-core/src/config.rs`: exactly the five schema-suffixed trees
  (`tokens.v1`/`costs.v1`/`latency.v1`/`traces.v1`/`counters.v1`),
  fixed-width BE keys, default `daemon_data_dir = ".hamlet-data/sled"`
  resolved against the working dir (config.rs:34), `InsideVillageTree`
  hard-reject (telemetry.rs:330); boundary + kill/restart tests green.

### QA suite (step 5) committed and green

`git ls-files` confirms all five artifacts are tracked and unmodified at
HEAD: `crates/hamlet-core/tests/adversarial_sanitize.rs` (7),
`adversarial_state.rs` (14), `crates/hamlet-cli/tests/adversarial_init.rs`
(13), `scripts/e2e-01-safename-compile-fail.sh`,
`scripts/e2e-01-topology-state.sh` — all executed green in this battery
(28/28 adversarial tests + both harnesses).

### Caveats

- Podman-guarded tests skipped (no podman on this host). Sprint 02+
  container work must re-run this battery on a podman-capable host/CI job
  (CONVENTIONS §5).

**GATE 01/06: PASS**
