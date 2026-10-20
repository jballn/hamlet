# HANDOFF — Sprint 07 Hibernation & Census

Objective: `OBJECTIVE.md`. Prompts: `01-architect` … `07-commit-agent`.

## Step 01 — architect: stasis formats + resource-bounds policy

Status: **done**. Commit: `feat(core): stasis schemas + resource policy types (ADR-0013)` → actual ADRs **ADR-0039** (stasis formats & census) and **ADR-0040** (resource bounds & hibernation policy). The prompt cites "ADR-0013", but 0013 is `0013-pkgcache-trust-model.md` (claimed in Sprint 02); per the `docs/adr/README.md` renumbering rule this work is ADR-0039/0040 and the renumber table was updated. **Later sprint-07 prompts reading "ADR-0013" must read ADR-0039/0040.**

### Done

- **`hamlet-core::stasis`** (SPEC §10, §14; ADR-0039, ADR-0040):
  - **Patron record schema (`PROPRIETOR<name>/`)** (ADR-0039 cl. 1–2):
    - `PatronFrontmatter` with machine-parseable YAML-like frontmatter metadata: `patron`, `register`, `blueprint`, `blueprint_version`, `source_uid`, optional `commission`, sorted `tags`, `created_at`, `stasis_version` (pinned to 1).
    - `PatronDoc`: parsed document model with `frontmatter` + freeform markdown `profile` body. Strict validation: LF line endings only, opening/closing fences (`---`), valid UIDs, byte limit 64 KiB (`MAX_PATRON_DOC_BYTES`).
    - Matching logic `matches_skill_config`: checks register, blueprint, and required skill tags against hibernated patron intelligence.
  - **`home.uid.bak/` backup integrity manifest** (ADR-0039 cl. 3):
    - `BackupManifest` and `ManifestEntry` (type: file/dir/symlink, FNV-1a/64 hex digest, size bytes, Unix permissions mode).
    - Canonical sorted JSON serialization and deserialization; cap 256 KiB (`MAX_MANIFEST_BYTES`); entry verification method (`verify_entry`) for tamper and drift detection.
  - **`TITLE.md` append-only chain-of-title grammar** (ADR-0039 cl. 4):
    - `TitleEntryKind` closed enum with 8 entry kinds: `trigger`, `provision`, `session`, `crash`, `steer`, `promotion`, `hibernate`, `awaken`.
    - `TitleEntry` typed variants carrying daemon-stamped lineage timestamps, tokens, and quoted escaped strings (`prompt`, `reason`, `directive`).
    - `TitleDoc`: parsed document structure; `parse` / `render_line` / `render` round-trip fidelity; `verify_append_only` asserting historical entries remain an exact unmodified prefix; `content_digest` FNV-1a tamper check.
  - **Dual convergence types** (SPEC §10, ADR-0039 cl. 5):
    - `BranchKind`: `DeedToBlueprint` (environment layer) and `ProprietorToPatron` (intelligence layer).
    - `BranchOutcome`: `Succeeded`, `Skipped`, `Failed` with separate tracking so failure in one branch does not block the other.
    - `DualConvergenceReport`: per-parcel report indicating success, failure, and escalation requirement.
    - `CommissionConvergenceSummary`: aggregated report across all parcel slots in a closing commission.
  - **Resource bounds policy types** (SPEC §14; ADR-0040 cl. 1–5):
    - `CandidateInput` and `HibernateCandidate`: scored hibernation candidate data structure.
    - Deterministic `rank_candidates` algorithm: prioritizes crashed (10,000) and stopped (8,000) agents over idle (5,000) and busy (0) agents; rewards winding-down commissions (+2,000), idle duration (up to 3,600 pts), memory footprint (+10 pts / MiB), crash history (+500 pts / crash); protects busy sessions and sessions with pending work.
    - Deterministic `evaluate_oom_policy`: OOM crash handling interplay with Sprint 03 restart budget (`RestartWithBackoff`, `SuggestEarlyHibernation` on repeated OOMs under >90% memory pressure, `BudgetExhaustedWakeMayor` on budget exhaustion).
    - Hard architectural rule: Daemon only **suggests** stasis by surfacing `WakeReason::HibernateCandidate` event to the Mayor; only a Mayor decree (`hibernate_agent`) or human operator CLI executes hibernation.
- **`hamlet-core::tree`**:
  - Added `patron_manifest(&SafeName) -> PathBuf` for `tavern/patrons/PROPRIETOR<name>/manifest.json`.
  - Added `manor_title() -> PathBuf` for `keep/census/manor/TITLE.md`.
  - Updated accessor tests in `crates/hamlet-core/src/tree.rs`.
- **`hamlet-core::packet`**:
  - Added `WakeReason::HibernateCandidate` (`"hibernate_candidate"`).
  - Added `EventKind::HibernateCandidate { uid, identity, memory_usage_bytes, memory_limit_bytes, idle_duration_secs, score }`.
  - Updated event header formatting and golden vectors.
- **`hamlet-core::config`** (v10 → v11, additive):
  - Bumped `CONFIG_VERSION` to 11.
  - Added `ResourcesConfig` fields: `cpu_shares: u64` (default 1024, range 2..=262144), `memory_pressure_threshold_pct: u8` (default 85, range 50..=99), `sample_window: usize` (default 3, range 1..=60), and `sample_interval_secs: u64` (default 5, range 1..=300).
  - Updated `crates/hamlet-cli/tests/golden/config.json` and core golden test.
  - Added migration test `v10_config_parses_with_default_resources_extensions`.

### Decisions (ADR refs)

- **ADR-0039 (Stasis Formats, Census & Dual Convergence)**:
  - Formats: Patron metadata as YAML frontmatter + Markdown body (`PROPRIETOR.md`); top-level file manifest in `manifest.json`; `TITLE.md` append-only bullet grammar (`- [timestamp] <kind> ...`).
  - Strict append-only invariant: historical entries in `TITLE.md` must never be modified or deleted.
  - Dual convergence: DEED-to-blueprint and PROPRIETOR-to-patron promotions are evaluated independently; each yields a `BranchOutcome`.
- **ADR-0040 (Resource Bounds, Pressure Detection & Hibernation Policy)**:
  - Podman limits: passed via `--memory`, `--cpus`, `--cpu-shares`, `--pids-limit`.
  - Detection: daemon samples container working set via podman over a rolling window (`sample_window`, `sample_interval_secs`); usage exceeding `memory_pressure_threshold_pct` triggers candidate evaluation.
  - Role separation: daemon evaluates pressure and surfaces candidates; Mayor decides and decrees hibernation.
  - OOM policy: backoff restarts under normal limits; early stasis suggested on repeated OOMs (>90% memory); Mayor woken when restart budget is exhausted.

### Verify

```sh
cargo test -p hamlet-core --lib stasis    # 8: patron round-trip/rejections, manifest round-trip, title grammar, dual convergence, candidate ranking, oom policy
cargo test -p hamlet-core --lib packet    # HibernateCandidate wake reason and event header table
cargo test -p hamlet-core --lib tree      # accessor paths
cargo test -p hamlet-core --lib config    # v10->v11 migration, resource bounds validation
cargo test -p hamlet-cli                 # init golden config.json matches v11
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-deps.sh
```

## Step 02 — daemon-engineer: TITLE chain-of-title + census records

Status: **done**. Commit: `feat(daemon): TITLE chain-of-title + census view (SPEC §10)`.

### Done

- **`hamlet-daemon::census` (TITLE Writer & Census Plane)** (SPEC §10, ADR-0039):
  - **Retrofit lifecycle event call sites across Sprints 02–06**:
    - Provisioning: stamped in `Provisioner::provision` (`TitleEntry::Trigger`, `TitleEntry::Provision`).
    - Session Start / Incarnation Restart: stamped in `supervise::supervise_lives` upon spawn and next incarnation (`TitleEntry::SessionIncarnation`).
    - Session Stopped / Killed: stamped in `supervise::supervise_lives` on session boundary exit (`TitleEntry::Promotion { kind: "session", target: session_token, status: "stopped"|"killed" }`).
    - Session Crashes / Failures: stamped in `supervise::supervise_lives` on restartable crashes (`TitleEntry::Crash`), and via `supervise::permanent_crash` on restart budget exhaustion and incarnation overflow (`TitleEntry::Crash`).
    - Steer / Divine Interrupts: stamped in `supervise::inject_interrupt` upon writing interrupt envelope to agent stdin (`TitleEntry::Steer { directive }`).
    - Git Bundle Mediation: stamped in `ledger::mediate_staged_bundle` upon accepted git bundle apply (`TitleEntry::Promotion { kind: "bundle", target: bundle_name, status: "applied" }`).
    - Deliverable Submission: stamped in `ledger::submit_deliverable` upon accepted freight deliverable (`TitleEntry::Promotion { kind: "submission", target, status: "submitted" }`).
    - DEED Evolution / Promotion: stamped in `promote::promote_parcel` on blueprint evolution (`status: "promoted"`) and on validation failure (`status: "rejected"`).
    - Vault Artifact Promotion: stamped in `vault::approve` (`status: "promoted"`) and `vault::reject` (`status: "rejected"`).
  - **Strict Append-Only Invariant & Tamper Detection**:
    - `stamp_parcel_title` verifies historical entries match unmodified prefix before appending.
    - Checksum-on-read tamper detection (`TitleDoc::verify_append_only`) detects unauthorized modifications or truncations.
    - Tamper evidence is non-fatal: records sled telemetry trace (`census-title-tamper`) and dispatches Mayor wake packet (`WakeReason::TitleTampered`, `EventKind::TitleTampered`).
    - In-memory `TITLE_CACHE` keyed by `(PathBuf, u32)` (`(tree.root().to_path_buf(), uid)`) guaranteeing cross-village test fixture isolation during parallel test execution.
  - **Manor Exemption**:
    - Manor parcel is exempt from hibernation; its chain-of-title lives permanently at `keep/census/manor/TITLE.md`.
    - Provisioned during `ManorProvisioner::provision` with `manor_title_stub`.
  - **Census View Listing API**:
    - Implemented `list_census(&HamletTree, &Charter, Option<&Telemetry>) -> Result<Vec<CensusParcelView>, CensusError>`.
    - Aggregates manor and parcel slots, current session/stasis state, commission slugs, last title event, title content digest, and sled telemetry counters (input/output tokens, cost, runtime, crashes).
    - Deterministic ASCII table formatter `render_agent_list(&[CensusParcelView]) -> String`.
- **`hamlet-cli::agent` (`hamlet agent list`)**:
  - Implemented `agent list [--commission <slug>] [--dir <dir>]` CLI subcommand with strict usage handling.
  - Formats census records into clean ASCII table; filters by commission slug if provided; supports `--dir` for custom village roots.
- **`hamlet-core::packet`**:
  - Added `WakeReason::TitleTampered` (`"title_tampered"`).
  - Added `EventKind::TitleTampered { uid, identity, detail }` with formatted event header and test coverage.

### Verify

```sh
cargo test -p hamlet-daemon --test census        # 3: golden TITLE sequence, tamper sled trace + Mayor wake, manor exempt listing
cargo test -p hamlet-cli --test agent_golden    # 4: CLI agent list table output, --commission filtering, --dir flag, usage errors
cargo test -p hamlet-daemon --test provision    # 14: provision lifecycle tests with escaped prompt assertions
cargo test -p hamlet-daemon --test supervise    # 14: session supervision lifecycle and title stamping
cargo test -p hamlet-daemon --test adversarial_agent # 12: adversarial agent crash and tamper resilience
cargo test -p hamlet-core --lib packet          # TitleTampered wake reason and event header table
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-deps.sh
```

### Next Steps (Step 03)
- Daemon engineer implements Step 03 (`roadmap/sprints/07-hibernation-census/03-daemon-engineer.md`): `Stasis` trait implementation, `hibernate(uid, trigger)` routine, patron stasis, manifest verification, and clean/crashed-hibernating state matrix.

## Step 03 — daemon-engineer: hibernation routine + patron stasis

Status: **done**. Commit: `feat(daemon): hibernation routine + patron stasis (SPEC §10)`.

### Done

- **`hamlet-daemon::census::ProductionStasis`** (SPEC §10; ADR-0039, ADR-0040):
  - **`Stasis` trait implementation & seam**:
    - Deleted the Sprint 05 `CleanStopStasis` stub from `decrees.rs` and wired `pub use crate::census::ProductionStasis;`.
    - Added `fn hibernate(&self, uid: u32, trigger: &str)` to `Stasis` trait, delegating by default to `self.park(uid)` for test stub compatibility.
    - Updated `DecreeExecutor::execute` handling of `Decree::HibernateAgent` to pass trigger `"decree:<decree_id>"` to `stasis.hibernate`.
  - **5-step `hibernate(uid, trigger)` routine**:
    - **Step 1: Drain & Teardown**: drains delivery queue within grace window via `Supervisor::stop_session(uid)` (if attached) or stops container via `runtime.stop`, followed by container teardown `runtime.remove(identity, true)`. Undelivered items in `courier/queues/<uid>.jsonl` and mail spools `courier/mail/<recipient>/` are preserved without truncation.
    - **Step 2: Atomic Home Backup**: copies parcel `home/` to `tavern/patrons/PROPRIETOR<name>/home.uid.bak/` via atomic staging (`.tmp_PROPRIETOR...` staging dir). Enforces `MAX_HOME_BACKUP_TOTAL_BYTES` (256 MiB) size guard during recursion. Computes deterministic recursive digests and POSIX modes into `manifest.json` (`BackupManifest`) and verifies top-level entries before committing.
    - **Step 3: Mutated Profile Serialization**: reads parcel `PROPRIETOR.md`, parses existing frontmatter or synthesizes `PatronFrontmatter` (associating register archetype, blueprint, version, source UID, commission) and writes `tavern/patrons/PROPRIETOR<name>/PROPRIETOR.md` (intelligence-layer promotion independent of DEED/blueprint promotion).
    - **Step 4: Charter & Census Recording**: updates charter with `slot.status = SlotStatus::Hibernated`, `lease.stasis = true`, `session.status = SessionStatus::Stopped` (UID mapping persists); stamps `TitleEntry::Hibernate { patron, reason }` into `TITLE.md`; records sled trace `agent-hibernated`; asserts compute freed (no running container or supervisor session remains).
    - **Step 5: Parcel Disposition & Archive**: archives parcel metadata (`TITLE.md`, `DEED.md`, `PROPRIETOR.md`) to `keep/archives/parcel<uid>/`, then releases `keep/census/parcel<uid>/` (`fs::remove_dir_all`).
- **Failure & Rollback Matrix** (SPEC §10, ADR-0039):
  - **Pre-flight / pre-teardown failure** (unknown slot UID, Manor exemption UID 1000, session stop error): rollback to pre-hibernation live state without modifying charter or creating patron directories.
  - **Post-teardown failure** (steps 2–5 failure after container teardown): container is already destroyed; transitions charter `slot.status = SlotStatus::Crashed` and `session.status = SessionStatus::Crashed`, stamps `TitleEntry::Crash` into `TITLE.md` describing post-teardown failure, and records `session-hibernation-failed` in sled telemetry. Returns typed `StasisError::PostTeardown`.
- **`hamlet-cli::agent` (`hamlet agent hibernate`)**:
  - Implemented `agent hibernate <uid> [--dir <dir>]` CLI subcommand with strict usage handling.
  - Rejects Manor UID 1000 (`uid 1000 is exempt from hibernation`), invalid integers, unknown slots, missing village roots, and unexpected arguments.
  - Updates usage help text in `hamlet`.
- **Integration Tests** (`crates/hamlet-daemon/tests/hibernate.rs` & `crates/hamlet-cli/tests/agent_golden.rs`):
  - `test_clean_hibernate_happy_path`: verifies full 5-step cycle, patron layout, manifest integrity, archive copies, charter states, sled trace, freed compute, and census reporting.
  - `test_hibernate_preserves_delivery_queue_and_mail`: validates queue persistence and spool retention.
  - `test_manifest_integrity_and_verification`: tests tamper detection on digests, sizes, and file types.
  - `test_rollback_preflight_on_manor_or_unknown_slot`: verifies zero side effects on Manor or invalid UIDs.
  - `test_post_teardown_failure_marks_crashed_hibernating`: tests overflow size guard triggering crash transition and title crash stamp.
  - `test_decree_executor_integration`: tests `Decree::HibernateAgent` through `DecreeExecutor::scan`.
  - Golden CLI tests: test `hamlet agent hibernate` parsing, Manor rejection, unknown slot refusal, and directory flags.

### Decisions (ADR refs)

- **ADR-0039 (Stasis Formats, Census & Dual Convergence)**:
  - Patron directory layout adheres strictly to `tavern/patrons/PROPRIETOR<name>/` containing `home.uid.bak/`, `PROPRIETOR.md`, and `manifest.json`.
  - Atomicity achieved via transient directory `.tmp_PROPRIETOR<name>_<uid>_<nanos>` within `tavern/patrons/` which is validated before being atomically renamed to `PROPRIETOR<name>/`.
  - When parcel directory `parcel<uid>/` is released in Step 5, `list_census` transparently falls back to `keep/archives/parcel<uid>/TITLE.md` to preserve historical visibility and census queries.
- **ADR-0040 (Hibernation Triggers & Failure Matrix)**:
  - Strict human/decree trigger model: only Mayor decrees (`Decree::HibernateAgent`) or operator CLI (`hamlet agent hibernate`) can initiate hibernation; the daemon never autonomously hibernates agents.
  - Split-recovery model: failures before container destruction are safe no-ops preserving live agent state; failures after container destruction cleanly mark the slot as `Crashed` (`crashed-hibernating`), preserving failure traces in `TITLE.md` and telemetry.

### Verify

```sh
cargo test -p hamlet-daemon --test hibernate    # 6: clean hibernate, queue preservation, manifest integrity, rollback, crash matrix, decree execution
cargo test -p hamlet-cli --test agent_golden    # 5: CLI agent list table output, commission filter, dir flag, usage errors, hibernate CLI
cargo test -p hamlet-daemon --test census        # 3: golden TITLE sequence, tamper sled trace + Mayor wake, manor exempt listing
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-deps.sh
```

### Next Steps (Step 04)
- Awakening routine (`awaken(patron, commission)`) & dual convergence (`04-daemon-engineer.md`).

## Step 04 — daemon-engineer: quick awakening + dual-state convergence

Status: **done**. Commit: `feat(daemon,cli): quick awakening + dual-state convergence (SPEC §10)`.

### Done

- **`hamlet-daemon::census` (Quick Awakening Routine)** (SPEC §10; ADR-0039):
  - Implemented `awaken(patron_ref, commission) -> Result<AwakenedAgent, StasisError>` and `awaken_sync`:
    - Validates patron record existence, `PROPRIETOR.md`, and `manifest.json`.
    - Tamper detection against `BackupManifest`: validates all file types, sizes, FNV-1a digests, and enforces ADR-0007 / ADR-0039 symlink prohibition. Detects extra untracked files or modified files.
    - Slot allocation with lineage preservation: allocates lowest free UID via `allocate_fresh_slot(patron_ref, commission)`; the patron's prior slot remains in `charter.slots` as historical lineage (`SlotStatus::Hibernated`).
    - Provision parcel *without clean bootstrap* (fast path): creates directory structure, copies `home.uid.bak/` into `parcel<uid>/home/` via `restore_dir_recursive`, seeds `PROPRIETOR.md` with new `source_uid` and commission, writes initial `DEED.md`, stamps fresh opening entry in `TITLE.md` (`TitleEntry::Awaken { patron, from_commission }`).
    - Container creation with derived image; skips toolchain download steps (`pkgcache` counters stay zero).
    - Session start with patron's intelligence profile injected as brief overlay.
    - Removes patron record from `tavern/patrons/` once awakening succeeds.
    - Mid-awaken failure safety: pre-flight or verification failures leave patron in `tavern/` completely intact and resumable.
  - Implemented `find_matching_patron` and `awaken_by_skill`: matches register archetype, blueprint, and required frontmatter tags per `matches_skill_config`.
  - Added `BranchOutcome::is_skipped` helper in `hamlet-core::stasis`.
  - Added `commission_convergence_archive` accessor to `HamletTree`.
- **`hamlet-daemon::ledger` (Dual-State Convergence at Commission End)** (SPEC §10; ADR-0039 cl. 5):
  - Updated `CommissionCloseReport`: added `pub convergence: Option<CommissionConvergenceSummary>`.
  - Updated `CommissionWake`: added `fn convergence_failed(&self, commission: u64, summary: &CommissionConvergenceSummary)`.
  - Added `stasis` and `runtime` builder and accessor methods to `Ledger` (`with_stasis`, `stasis`, `with_runtime`, `runtime`).
  - Rewired `Ledger::close_commission`:
    - For each rostered parcel UID, independently evaluates:
      - Branch 1: DEED -> blueprint via `promote_parcel_deed(uid)`.
      - Branch 2: PROPRIETOR -> patron via `stasis.hibernate_sync(uid, trigger)`.
    - Independent execution: failure in Branch 1 does not block Branch 2 and vice versa.
    - Aggregates both outcomes into `DualConvergenceReport` and `CommissionConvergenceSummary`.
    - Atomically writes summary to `keep/archives/commission<id>-convergence.json`.
    - If any branch fails, surfaces failure: files an `EscalationNotice` in `keep/census/manor/escalations/`, calls `CommissionWake::convergence_failed`, and traces to sled telemetry (`commission-convergence-failed`).
    - Reloads charter to absorb stasis updates, applies `CommissionEvent::OperatorClose`, sets `parcels[uid].stasis = true`, and exports final telemetry archive.
- **`hamlet-cli::agent` (`hamlet agent wake`)**:
  - Implemented `agent wake <patron> [--commission <slug>] [--dir <dir>]` CLI subcommand with strict usage handling.
  - Updates usage help text in `hamlet` main binary.
- **Integration Tests** (`crates/hamlet-daemon/tests/awaken.rs` & `crates/hamlet-cli/tests/agent_golden.rs`):
  - `test_hibernate_awaken_round_trip`: verifies full hibernate -> awaken cycle, home file checksum preservation, updated PROPRIETOR lineage, TITLE opening entry, and tavern cleanup.
  - `test_awaken_fast_path_zero_downloads`: verifies `pkgcache` remains completely zero during awakening.
  - `test_awaken_tamper_detection`: tests content modification, untracked file addition, and symlink injection rejections.
  - `test_restart_mid_awaken_resumable`: verifies patron remains intact on pre-flight error and awakens successfully on retry.
  - `test_dual_state_convergence_matrix`: tests matrix of convergence outcomes (both ok / deed fails / proprietor fails), failure isolation, escalation notices, and convergence archive persistence.
  - Golden CLI tests: test `hamlet agent wake` argument parsing, missing argument errors, unknown patron errors, and village not found errors.

### Decisions (ADR refs)

- **ADR-0039 (Stasis Formats, Census & Dual Convergence)**:
  - Lineage preservation: historical slots persist in `charter.slots` with `SlotStatus::Hibernated` or `SlotStatus::Crashed`. `charter.validate()` ensures identity uniqueness across *active* slots only (`status != Hibernated && status != Crashed`).
  - Dual-state convergence runs both promotion branches (DEED environment layer + PROPRIETOR intelligence layer) independently. Outcomes are serialized to `keep/archives/commission<id>-convergence.json`.
  - Tamper detection fails closed if files are modified, symlinks are inserted, or untracked files are added to `home.uid.bak/`.

### Verify

```sh
cargo test -p hamlet-daemon --test awaken       # 5: round-trip, fast path, tamper detection, resumable, dual convergence matrix
cargo test -p hamlet-daemon --test hibernate    # 6: clean hibernate, queue preservation, manifest integrity, rollback, crash matrix, decree execution
cargo test -p hamlet-daemon --test vault        # 5: vault promotions, operator close
cargo test -p hamlet-cli --test agent_golden    # 7: CLI agent list, hibernate, wake, usage errors
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-deps.sh
```

### Next Steps (Step 05)
- QA Engineer adversarial stasis suite & harness (`05-qa-engineer.md`).

## Step 05 — qa-engineer: adversarial stasis suite + e2e harness (SPEC §10, §14; ADR-0039, ADR-0040)

Status: **done, gate FAIL on 3 HIGH + 3 MEDIUM findings**. Commit:
`test(daemon,scripts): adversarial stasis suite + e2e harness + findings (SPEC §10, §14)`.
Strict role boundary maintained: **zero product code was modified**. Only test suites, e2e harnesses, and documentation were written.

### Committed Artifacts

- **`crates/hamlet-daemon/tests/adversarial_stasis.rs`** — 26 attack, lifecycle, race, and invariant tests covering all 7 mandated surface areas from `05-qa-engineer.md` plus live reproductions of QA07-01 through QA07-06. Uses `FakeRuntime` and isolated test trees; zero real podman dependency.
  - Surface areas covered:
    1. **TITLE integrity**: golden lifecycle sequence, strict append-only prefix verification, restart stability across cache clears, append on tampered title (QA07-04), cache loss on restart (QA07-05).
    2. **Hibernation races**: in-flight task queue preservation without truncation, interrupt safety, double-hibernate decree idempotence, crashed session hibernation allowed and marked, kill -9 mid-staging leaves pre-hibernation state intact, post-teardown failure transition to `Crashed`.
    3. **Stasis integrity**: file content corruption in `home.uid.bak/` refused at awaken with typed `ManifestVerification`, PROPRIETOR frontmatter tampering rejected at awaken, symlinks bricking awaken (QA07-02), dangling symlinks crashing post-teardown dir digest (QA07-06).
    4. **Awakening**: full slot space exhaustion hard error (`SlotError::Exhausted`), released old parcel unreadable, pkgcache counters zero fast path, multi-awaken patron destruction (QA07-01).
    5. **Convergence independence**: DEED promotion failure does not block PROPRIETOR save and vice versa, both failures emit escalation notice and wake event exactly once.
    6. **Resource policy**: static audit of all `.hibernate(` call sites, unauthorized trigger acceptance (QA07-03), candidate ranking algorithm (crashed > busy), OOM exponential backoff and early stasis vectors.
    7. **Full-cycle resumability**: provision → work → hibernate → restart daemon → awaken → converge → close, with deterministic kill points matching un-interrupted final state.
    8. **Defect reproductions**: live repro tests for QA07-01 through QA07-06.
- **`scripts/e2e-07-stasis.sh`** (chmod 0755) — 4-tier executable end-to-end stasis harness:
  - **T1**: CLI-level full lifecycle flow (`hamlet init` → `commission new` → provision worker scout UID 1001 → `agent list` verify running → `agent hibernate 1001` verify stasis → `agent wake scout` verify new UID 1002 allocated with preserved stasis predecessor → deliverable and vault promotion → `commission close` → telemetry archives asserted). Includes subuid unshare clean-up trap.
  - **T2**: Adversarial stasis test suite (26 tests, all green).
  - **T3**: Source-level ratchets for Sprint 07 invariants (typed `StasisError` variants, `BackupManifest` digest & `PATRON_SCHEMA_VERSION = 1`, normative size caps, Manor UID 1000 exemption, fresh slot allocation on awaken, dual convergence independence in `ledger.rs`, `CONFIG_VERSION = 11` memory pressure threshold, candidate ranking & OOM policy).
  - **T4**: Guarded podman check (`ci_guarded_direct_viable`, podman-optional).

---

### What the Suite Proved GREEN

1. **TITLE Integrity Invariants (ADR-0039 cl. 4, SPEC §10)**:
   - Full golden lifecycle sequence (`trigger` → `provision` → `session` → `steer` → `promotion` → `crash` → `hibernate` → `awaken`) renders and parses round-trip with byte-exact fidelity.
   - `stamp_parcel_title` verifies historical entries match unmodified prefix before appending.
   - Checksum-on-read tamper detection (`TitleDoc::verify_append_only`) detects unauthorized modifications and dispatches Mayor wake packet (`WakeReason::TitleTampered`).
   - Chain-of-title parses identically across simulated daemon restarts.
2. **Hibernation Race Invariants (SPEC §10, ADR-0040)**:
   - Hibernating an agent while a task is in-flight preserves the courier delivery queue intact without truncation.
   - Hibernating an agent during a live interrupt safely drains the interrupt envelope into stasis.
   - Repeated hibernation calls on an already-hibernated agent are strictly idempotent no-ops returning the existing `PatronDoc`.
   - Hibernating a crashed session is explicitly permitted and leaves a clean stasis parcel.
   - A simulated kill -9 mid-staging leaves the pre-hibernation parcel state intact, permitting clean retry.
   - Teardown failures occurring after container removal correctly transition the parcel to `Crashed` status with audit trail in `TITLE.md`.
3. **Stasis Integrity Invariants (ADR-0039 cl. 2–3)**:
   - Tampering with even a single byte of a file in `home.uid.bak/` is caught by `verify_backup_dir` at awaken time and refused with typed `StasisError::ManifestVerification`.
   - Tampering with YAML frontmatter in `PROPRIETOR.md` (invalid keys, altered tags, malformed schema) is refused at awaken time.
4. **Awakening Invariants (SPEC §10, ADR-0039)**:
   - Awakening into a fully exhausted slot space (`FIRST_AGENT_UID..=LAST_AGENT_UID` occupied) fails closed with hard typed `StasisError::Slot(SlotError::Exhausted)`.
   - Once awakened into a fresh slot, the old parcel directory is released and inaccessible to the awakened agent.
   - `pkgcache` counters remain strictly zero during awakening, proving the fast path skips base image toolchain downloads.
5. **Convergence Independence Invariants (ADR-0039 cl. 5, SPEC §10)**:
   - DEED promotion failure does not block PROPRIETOR save to patron stasis.
   - PROPRIETOR save failure does not block DEED promotion to blueprint.
   - Simultaneous failure of both branches files an `EscalationNotice` in `keep/census/manor/escalations/` and calls `CommissionWake::convergence_failed` exactly once.
6. **Resource Policy & Candidate Ranking Invariants (ADR-0040, SPEC §14)**:
   - Static audit confirms all `.hibernate(` call sites in `crates/` are strictly confined to `census.rs`, `decrees.rs`, `ledger.rs`, and `agent.rs`.
   - Candidate ranking prioritizes crashed (score > 10,000) and stopped agents over active sessions with pending tasks.
   - OOM policy correctly calculates exponential backoff under normal memory limits, suggests early stasis under critical pressure (>90%), and exhausts budget to wake the Mayor.
7. **Full-Cycle Resumability (SPEC §10)**:
   - Provision → work → hibernate → restart daemon → awaken → converge → close executed with kills at 3 deterministic points (mid-hibernate staging, post-daemon restart, pre-close) results in a final village tree state identical to an uninterrupted golden run.

---

### Findings

#### QA07-01 — HIGH — `awaken` Destroys Patron Record and Prevents Multi-Awaken Lineage

- **Location**: `crates/hamlet-daemon/src/census.rs:1856-1875, 2054`
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_stasis qa07_01`
- **Expected**:
  - SPEC §10: "two awakens of one patron → two independent identities, shared lineage refs".
  - Step 05 QA Requirement 4: "two awakens of one patron → two independent identities, shared lineage refs".
  - A patron in `tavern/patrons/PROPRIETOR<name>/` represents reusable template intelligence that can be instantiated across multiple commissions or parallel tasks with distinct identities and shared lineage.
- **Actual**:
  - In `census.rs:2054`, upon successful awakening, `awaken_internal` executes `let _ = fs::remove_dir_all(&patron_dir);`. This unconditionally deletes the patron record from `tavern/patrons/`. A second attempt to awaken that patron immediately fails with `StasisError::NoSuchPatron`.
  - Furthermore, in `allocate_fresh_slot` (line 1863), `let identity = patron_ref.clone();` assigns the exact patron name as the new agent identity. If the previous agent awakened from this patron is still active in the charter, `charter.allocate_slot` fails with `SlotError::IdentityActive(identity)` due to `charter.validate()` rejecting duplicate active identities.
- **Remediation**:
  - Do not unconditionally remove `patron_dir` on wake, or preserve template patrons for multi-instantiation.
  - When awakening a patron multiple times, generate unique instance identities (e.g. `patron-1`, `patron-2`) while recording shared lineage back to `patron_ref` in `PROPRIETOR.md` and `TITLE.md`.

#### QA07-02 — HIGH — `copy_dir_recursive` Copies Symlinks, Creating Poisoned Backups That Brick Subsequent Awakening

- **Location**: `crates/hamlet-daemon/src/census.rs:2565-2574, 2233-2240, 2610-2614`
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_stasis qa07_02`
- **Expected**:
  - ADR-0007 / ADR-0039 cl. 3: Symlinks in `home/` must never compromise village security or cause unrecoverable state. If symlinks are forbidden, hibernation should reject them up-front *before* destroying the container and deleting the live parcel.
- **Actual**:
  - In `copy_dir_recursive` (lines 2565-2574), when encountering a symlink, it copies it via `fs::read_link` and `unix::fs::symlink`.
  - `hibernate_internal` builds `manifest.json` recording the symlink entry (`ManifestEntryType::Symlink`), removes the container, and deletes the live parcel directory (`fs::remove_dir_all(&parcel_dir)`).
  - However, during subsequent `awaken` (`verify_backup_dir`, lines 2610-2614), the validator strictly checks: `if meta.file_type().is_symlink() || entry.entry_type == ManifestEntryType::Symlink { return Err(StasisError::ManifestVerification("refusing symlink ...")); }`.
  - Because the live parcel was already deleted and container removed, the patron backup is permanently poisoned and un-awakenable, causing permanent data loss and bricking the patron.
- **Remediation**:
  - Enforce ADR-0007 symlink refusal up-front during `hibernate_internal` pre-flight *before* tearing down compute and deleting the live parcel, or sanitize/skip symlinks during backup staging.

#### QA07-03 — HIGH — Missing Runtime Assertion on Hibernation Trigger

- **Location**: `crates/hamlet-daemon/src/census.rs:2066-2074`, `crates/hamlet-daemon/src/ledger.rs:2059-2060`
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_stasis qa07_03`
- **Expected**:
  - Step 05 QA Requirement 6: "daemon NEVER auto-hibernates (audit all hibernate call sites: trigger must be decree|operator — static test over the code path + runtime assertion)".
  - SPEC §14 & ADR-0040: Only explicit Mayor decrees (`decree:...`) or human operator CLI actions (`operator:...`) may initiate agent hibernation.
- **Actual**:
  - `hibernate_internal` accepts any arbitrary `&str` without validating prefix `decree:` or `operator:`.
  - In `crates/hamlet-daemon/src/ledger.rs:2059`, `close_commission` calls `stasis.hibernate_sync(uid, &trigger)` with `let trigger = format!("commission:{}:close", commission_slug);`, passing a `commission:*` prefix that violates the mandatory `decree|operator` trigger policy.
- **Remediation**:
  - Add a runtime check in `ProductionStasis::hibernate_internal`: `if !trigger.starts_with("decree:") && !trigger.starts_with("operator:") { return Err(StasisError::UnauthorizedTrigger(trigger.to_string())); }`.
  - Update `ledger.rs:2059` to use an authorized trigger string conforming to the policy (e.g. `operator:commission:{}:close`).

#### QA07-04 — MEDIUM — `TitleWriter::append` Ignores `read_and_verify` Tamper Failures

- **Location**: `crates/hamlet-daemon/src/census.rs:1030-1033`
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_stasis qa07_04`
- **Expected**:
  - SPEC §10 & Step 05 QA Requirement 1: "agent-side TITLE.md edits detected; daemon never rewrites history (append-only assertion over full lifecycle)".
  - When appending to `TITLE.md`, if the existing on-disk document fails `verify_append_only` (indicating unauthorized truncation or modification), the write should refuse to proceed on top of a compromised history and emit an alarm.
- **Actual**:
  - In `TitleWriter::append`:
    ```rust
    // Verify against cache if present, and update cache with current read
    let _ = self.read_and_verify(uid);
    ```
  - `read_and_verify` detects the tamper and fires the Mayor wake, but `append` discards the `Result` with `let _ = ...;` and proceeds to blindly format and append the entry to the tampered file.
- **Remediation**:
  - In `TitleWriter::append`, handle the result of `read_and_verify(uid)`. If it returns a tamper or parse error, fail closed or handle the corrupted state explicitly instead of discarding the error.

#### QA07-05 — MEDIUM — Ephemeral In-Memory `TITLE_CACHE` Blind to Tampering Across Daemon Restarts

- **Location**: `crates/hamlet-daemon/src/census.rs:95-97, 1008-1017`
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_stasis qa07_05`
- **Expected**:
  - Tamper detection should persist across daemon restarts (e.g. comparing `TITLE.md` digest against persisted sled telemetry traces or charter receipts).
- **Actual**:
  - `TITLE_CACHE` is a process-memory `OnceLock<Mutex<HashMap<(PathBuf, u32), (String, u64)>>>`.
  - On daemon restart or process reboot, `TITLE_CACHE` is empty. The first read of a tampered `TITLE.md` on reboot populates the cache with the tampered content, allowing unauthorized modifications made while the daemon was offline to evade detection.
- **Remediation**:
  - Cross-check `TITLE.md` content digest or entry count against persisted sled telemetry or charter records on startup.

#### QA07-06 — MEDIUM — `compute_dir_digest` Follows Symlinks and Fails on Dangling Links

- **Location**: `crates/hamlet-daemon/src/census.rs:2733`
- **Repro Command**: `cargo test -p hamlet-daemon --test adversarial_stasis qa07_06`
- **Expected**:
  - Computing directory digests should use `entry.symlink_metadata()` (lstat) rather than `entry.metadata()` (stat).
- **Actual**:
  - Line 2733 calls `entry.metadata()?`. If a subdirectory contains a dangling symlink (broken link), `entry.metadata()` fails with `NotFound`.
  - Because this happens in `compute_dir_digest` after the container was already torn down, it triggers `handle_post_teardown_failure`, permanently branding the parcel as `Crashed` even though no actual container crash occurred.
- **Remediation**:
  - Use `entry.symlink_metadata()` in `compute_dir_digest`.

---

### Gate Verdict

**GATE 07/05: FAIL**

The adversarial suite and e2e harness are committed and all 26 tests pass cleanly. However, gate 07/05 fails due to **3 HIGH** and **3 MEDIUM** defect findings in product code:
1. `awaken` unconditionally deletes patron records and hardcodes single-identity allocation, preventing multi-awaken lineage (QA07-01).
2. `copy_dir_recursive` allows symlinks during hibernation backup, which subsequent `awaken` rejects, permanently bricking the patron (QA07-02).
3. Missing runtime assertion on hibernation trigger, and `close_commission` passes an unauthorized trigger (QA07-03).
4. `TitleWriter::append` ignores `read_and_verify` tamper failures (QA07-04).
5. Ephemeral `TITLE_CACHE` blind to offline tampering across daemon restarts (QA07-05).
6. `compute_dir_digest` uses `metadata()` instead of `symlink_metadata()`, crashing on dangling links (QA07-06).

Remediation by the daemon engineer in Step 05b is required before proceeding to Step 06.



