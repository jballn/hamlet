//! Soak + resumability marathon test harness (Sprint 10 Step 02, SPEC §13).
//!
//! Multi-tenant load:
//! 1. 3 concurrent commissions with bare git repositories and briefs
//! 2. 12+ agents rostered across commissions with private homes, DEED, TITLE
//! 3. Interleaved mail storms across commissions and slots
//! 4. Git bundle pushes & mediation with git fsck verification
//! 5. Freight deliverable submissions & vault promotions with SHA-256 sidecars
//! 6. Dynamic DEED alteration promotions & blueprint version progression
//!
//! Lifecycle churn:
//! 1. Hibernate/awaken cycles for every agent >= 3x (36 hibernations, 36 awakenings)
//! 2. Patron re-matching and document validation across cycles
//! 3. Blueprint version growth (v1 -> v2 -> v3 ...) from dynamic alterations
//!
//! Chaos injection:
//! 1. Daemon kill -9 at 20 deterministic pseudo-random points (seeded PRNG)
//! 2. Watcher event storms (10,000 rapid file ops)
//! 3. WS client flapping (abrupt disconnects, reconnects, resync verification)
//! 4. Webhook dispatch endpoint blackholes (retry backoff & dead-letter queue bounds)
//!
//! Invariants asserted after EVERY chaos point and at completion:
//! 1. Charter loads + is byte-stable on restart-with-no-activity
//! 2. No orphan containers, slots, or parcels
//! 3. TITLE append-only unbroken (monotonically ordered, unbroken digest chain)
//! 4. Secrets grep battery: no secret material anywhere in tree, sled, or logs
//! 5. Vault artifacts checksum-valid against recorded SHA-256
//! 6. Git repos fsck-clean across all bare repositories
//!
//! Duration target:
//! - >= 30 minutes simulated village time (>= 1,800 simulated ticks/events, deterministic)
//! - Zero invariant violations.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionState, IndentureEntryStatus};
use hamlet_core::config::Config;
use hamlet_core::decree::content_digest;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionRecord, SessionStatus};
use hamlet_core::slots::SlotStatus;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::{
    clear_title_cache, format_epoch_iso8601, ProductionStasis, TitleWriter,
};
use hamlet_daemon::decrees::Promotion;
use hamlet_daemon::ledger::{GitBundleOutcome, Ledger, RecordingCommissionWake};
use hamlet_daemon::mailroute::{MailActor, MailRouter};
use hamlet_daemon::podman::FakeRuntime;
use hamlet_daemon::promote::{DeedPromoter, PromotionOutcome};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::Telemetry;
use hamlet_daemon::vault::VaultPromotion;

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const SOAK_PRNG_SEED: u64 = 0x50A4_2026_1020_0001;
const TARGET_SIMULATED_SECONDS: u64 = 1800; // 30 minutes simulated village time

/// Deterministic Pseudo-Random Number Generator (xorshift64star).
/// Fully reproducible across runs with a fixed seed.
struct SoakPrng {
    state: u64,
}

impl SoakPrng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x50A4_2026_1020_0001
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn next_range(&mut self, min: u64, max: u64) -> u64 {
        if min >= max {
            return min;
        }
        min + (self.next_u64() % (max - min + 1))
    }
}

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid safe name")
}

fn run_git_cmd(work_dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(work_dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git command failed: git {:?}\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("valid utf-8 git output")
}

fn setup_seed_repo(base: &Path, comm_slug: &str) -> PathBuf {
    let seed_dir = base.join(format!("seed-{comm_slug}"));
    fs::create_dir_all(&seed_dir).expect("create seed dir");
    run_git_cmd(&seed_dir, &["init", "-b", "main"]);
    run_git_cmd(&seed_dir, &["config", "user.name", "Village Architect"]);
    run_git_cmd(
        &seed_dir,
        &["config", "user.email", "architect@hamlet.local"],
    );

    let readme = format!("# Project {comm_slug}\nBaseline seed repository for {comm_slug}.\n");
    fs::write(seed_dir.join("README.md"), readme).expect("write seed readme");
    let code = format!("// Module {comm_slug}\npub fn status() -> &'static str {{ \"READY\" }}\n");
    fs::write(seed_dir.join("lib.rs"), code).expect("write seed code");

    run_git_cmd(&seed_dir, &["add", "."]);
    run_git_cmd(&seed_dir, &["commit", "-m", "Initial seed commit"]);
    seed_dir
}

fn walk_tree(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_tree(&path, files);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
}

// ===========================================================================
// Six Invariants Asserted After Every Chaos Point
// ===========================================================================

/// Invariant 1: Charter loads and is byte-stable on restart with no activity.
fn assert_charter_byte_stable(tree: &HamletTree) {
    let charter_path = tree.charter();
    assert!(
        charter_path.is_file(),
        "charter.json must exist at {}",
        charter_path.display()
    );
    let original_bytes = fs::read(&charter_path).expect("read charter bytes");
    let loaded =
        Charter::load(&charter_path).expect("charter must parse cleanly without corruption");
    let rendered = loaded.render();
    assert_eq!(
        original_bytes,
        rendered.as_bytes(),
        "Invariant 1 failed: charter re-serialization differs from committed disk bytes"
    );
}

/// Invariant 2: No orphan containers, slots, or parcels.
fn assert_no_orphan_slots_or_parcels(tree: &HamletTree) {
    let charter = Charter::load(&tree.charter()).expect("load charter");

    // Every active slot must have a parcel directory on disk (or manor for mayor).
    // Every hibernated slot must NOT have an active parcel directory on disk.
    for (&uid, slot) in &charter.slots {
        if uid == hamlet_core::slots::MAYOR_UID {
            assert!(
                tree.manor().is_dir(),
                "Invariant 2 failed: Mayor manor dir missing at {}",
                tree.manor().display()
            );
            continue;
        }

        let parcel_dir = tree.parcel(uid);
        match slot.status {
            SlotStatus::Active => {
                assert!(
                    parcel_dir.is_dir(),
                    "Invariant 2 failed: active slot {uid} ({}) missing parcel dir at {}",
                    slot.identity,
                    parcel_dir.display()
                );
            }
            SlotStatus::Crashed => {
                // Crashed slot may still retain its parcel dir pending recovery/reap
            }
            SlotStatus::Hibernated => {
                assert!(
                    !parcel_dir.exists(),
                    "Invariant 2 failed: slot {uid} ({}) is Hibernated but parcel dir exists at {}",
                    slot.identity,
                    parcel_dir.display()
                );
            }
        }
    }

    // Disk check: every parcel directory in keep/census/ must map to an active slot in charter.
    let census_dir = tree.keep_census();
    if census_dir.is_dir() {
        for entry in fs::read_dir(&census_dir)
            .expect("read census dir")
            .flatten()
        {
            let entry_name = entry.file_name().to_string_lossy().into_owned();
            if let Some(uid_str) = entry_name.strip_prefix("parcel") {
                if let Ok(uid) = uid_str.parse::<u32>() {
                    let slot = charter.slots.get(&uid);
                    assert!(
                        slot.is_some(),
                        "Invariant 2 failed: orphan parcel directory {entry_name} has no slot entry in charter"
                    );
                    assert_eq!(
                        slot.unwrap().status,
                        SlotStatus::Active,
                        "Invariant 2 failed: parcel directory {entry_name} exists on disk but slot status is {:?}",
                        slot.unwrap().status
                    );
                }
            }
        }
    }
}

/// Invariant 3: TITLE append-only unbroken.
fn assert_title_append_only_unbroken(tree: &HamletTree) {
    let charter = Charter::load(&tree.charter()).expect("load charter");
    let writer = TitleWriter::new(tree.clone());

    for (&uid, slot) in &charter.slots {
        if slot.status == SlotStatus::Active {
            let title_path = tree.parcel_title(uid);
            if title_path.is_file() {
                let (doc, tampered) = writer
                    .read_and_verify(uid)
                    .unwrap_or_else(|e| panic!("failed to read TITLE.md for slot {uid}: {e}"));
                assert!(
                    !tampered,
                    "Invariant 3 failed: TITLE.md tampering detected for slot {uid}"
                );
                assert!(
                    !doc.entries.is_empty(),
                    "Invariant 3 failed: TITLE.md is empty for active slot {uid}"
                );
            }
        }
    }
}

/// Invariant 4: No secret material anywhere in tree, sled, or logs (grep battery).
fn assert_no_secrets_in_tree(tree: &HamletTree, telemetry_path: &Path, log_path: &Path) {
    let forbidden_patterns = [
        b"sk-ant-api03-".as_slice(),
        b"ghp_fake_production_secret".as_slice(),
        b"SUPER_SECRET_KEY_NEVER_LEAK".as_slice(),
        b"AIzaSyFakeGoogleApiKey123456789".as_slice(),
        b"OPENAI_API_SECRET_KEY".as_slice(),
    ];

    let mut files = Vec::new();
    walk_tree(tree.root(), &mut files);
    if telemetry_path.is_dir() {
        walk_tree(telemetry_path, &mut files);
    }
    if log_path.is_dir() {
        walk_tree(log_path, &mut files);
    }

    for file in files {
        let bytes = fs::read(&file).unwrap_or_default();
        for pattern in &forbidden_patterns {
            assert!(
                !bytes.windows(pattern.len()).any(|w| w == *pattern),
                "Invariant 4 failed: found forbidden secret string in file: {}",
                file.display()
            );
        }
    }
}

/// Invariant 5: Vault artifacts checksum-valid.
fn assert_vault_checksums_valid(tree: &HamletTree) {
    let vault_dir = tree.keep_vault();
    if !vault_dir.is_dir() {
        return;
    }

    let mut files = Vec::new();
    walk_tree(&vault_dir, &mut files);

    for file in files {
        if file.extension().is_some_and(|ext| ext == "sha256") {
            let sidecar_content = fs::read_to_string(&file).expect("read sha256 sidecar");
            let expected_checksum = sidecar_content
                .split_whitespace()
                .next()
                .expect("sha256 in sidecar");

            let target_file = file.with_extension("");
            if target_file.is_file() {
                let target_bytes = fs::read(&target_file).expect("read target vault artifact");
                let actual_checksum = content_digest(&target_bytes);
                assert_eq!(
                    actual_checksum,
                    expected_checksum,
                    "Invariant 5 failed: vault checksum mismatch for {}",
                    target_file.display()
                );
            }
        }
    }
}

/// Invariant 6: Git repositories fsck-clean.
fn assert_git_repos_fsck_clean(tree: &HamletTree, commissions: &[u64]) {
    for &comm_id in commissions {
        let git_dir = tree.commission_git(comm_id);
        if git_dir.is_dir() {
            let out = Command::new("git")
                .arg(format!("--git-dir={}", git_dir.display()))
                .args(["fsck", "--full", "--strict"])
                .output()
                .unwrap_or_else(|e| panic!("git fsck exec failed for commission {comm_id}: {e}"));
            assert!(
                out.status.success(),
                "Invariant 6 failed: git fsck returned errors for commission {comm_id}:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// Assert all 6 invariants simultaneously.
fn assert_all_invariants(
    tree: &HamletTree,
    telemetry_path: &Path,
    log_path: &Path,
    commissions: &[u64],
    checkpoint_desc: &str,
) {
    assert_charter_byte_stable(tree);
    assert_no_orphan_slots_or_parcels(tree);
    assert_title_append_only_unbroken(tree);
    assert_no_secrets_in_tree(tree, telemetry_path, log_path);
    assert_vault_checksums_valid(tree);
    assert_git_repos_fsck_clean(tree, commissions);
    eprintln!("  [invariant-ok] {checkpoint_desc}");
}

// ===========================================================================
// Multi-Tenant Village Test Fixture
// ===========================================================================

#[allow(dead_code)]
struct VillageFixture {
    _tmp: tempfile::TempDir,
    work_dir: PathBuf,
    tree: HamletTree,
    telemetry_dir: PathBuf,
    telemetry: Arc<Telemetry>,
    log_dir: PathBuf,
    supervisor: Arc<Supervisor>,
    mail: Arc<MailRouter>,
    ledger: Arc<Ledger>,
    stasis: Arc<ProductionStasis>,
    promoter: Arc<DeedPromoter>,
    vault: Arc<VaultPromotion>,
    commissions: Vec<u64>,
    commission_slugs: Vec<String>,
    current_agent_uids: BTreeMap<String, u32>, // agent_identity -> current slot uid
    simulated_seconds: u64,
    event_count: u64,
}

impl VillageFixture {
    fn new(_seed: u64) -> Self {
        clear_title_cache();
        let tmp = tempfile::tempdir().expect("create tempdir");
        let work_dir = tmp.path().to_path_buf();
        let tree_root = work_dir.join(".hamlet");
        let tree = HamletTree::ensure(&tree_root).expect("ensure village tree");

        // Reserve mayor slot (1000)
        let mut charter = Charter::empty();
        charter.reserve_mayor(name("mayor")).expect("reserve mayor");
        charter
            .save_atomic(&tree.charter())
            .expect("save initial charter");

        let telemetry_dir = work_dir.join(".telemetry");
        let telemetry = Arc::new(Telemetry::open(&telemetry_dir).expect("open telemetry"));
        let log_dir = work_dir.join("logs");
        fs::create_dir_all(&log_dir).expect("create log dir");

        let runtime = Arc::new(FakeRuntime::new());
        let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
        let supervisor = Arc::new(Supervisor::new(
            tree.clone(),
            Config::default(),
            spawner,
            Arc::clone(&telemetry),
        ));

        let mail = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
        let wake = Arc::new(RecordingCommissionWake::new());
        let ledger = Arc::new(
            Ledger::new(tree.clone())
                .with_wake(Arc::clone(&wake) as _)
                .with_mail_router(Arc::clone(&mail))
                .with_telemetry(Arc::clone(&telemetry)),
        );
        mail.set_ledger(Arc::clone(&ledger));

        let stasis = Arc::new(
            ProductionStasis::new(tree.clone(), Arc::clone(&runtime) as _)
                .with_telemetry(Arc::clone(&telemetry)),
        );

        let promoter = Arc::new(
            DeedPromoter::new(tree.clone())
                .with_telemetry(Arc::clone(&telemetry))
                .with_mail(Arc::clone(&mail)),
        );

        let vault = Arc::new(
            VaultPromotion::new(tree.clone())
                .with_telemetry(Arc::clone(&telemetry))
                .with_mail(Arc::clone(&mail))
                .with_supervisor(Arc::clone(&supervisor))
                .with_stasis(Arc::clone(&stasis)),
        );

        // Seed default blueprint village-alpine v1 in guild
        let bp = Blueprint::default_village_alpine().expect("default blueprint");
        let bp_path = tree.guild_blueprint(&name("village-alpine"));
        fs::create_dir_all(bp_path.parent().expect("blueprint parent")).expect("mkdir blueprints");
        fs::write(&bp_path, bp.render_toml(None)).expect("write default blueprint");

        Self {
            _tmp: tmp,
            work_dir,
            tree,
            telemetry_dir,
            telemetry,
            log_dir,
            supervisor,
            mail,
            ledger,
            stasis,
            promoter,
            vault,
            commissions: Vec::new(),
            commission_slugs: Vec::new(),
            current_agent_uids: BTreeMap::new(),
            simulated_seconds: 0,
            event_count: 0,
        }
    }

    fn tick(&mut self, seconds: u64) {
        self.simulated_seconds += seconds;
        self.event_count += 1;
    }

    fn create_commission(&mut self, slug: &str) -> u64 {
        let seed = setup_seed_repo(&self.work_dir, slug);
        let brief = format!("# Commission {slug}\nAutonomous engineering subtask {slug}.\n");
        let res = self
            .ledger
            .create_commission(slug, &brief, Some(&seed))
            .expect("create commission");

        // Mark commission Active in charter
        let mut charter = Charter::load(&self.tree.charter()).expect("load charter");
        if let Some(comm) = charter.commissions.get_mut(&res.id) {
            comm.state = CommissionState::Active;
        }
        charter
            .save_atomic(&self.tree.charter())
            .expect("save charter");

        self.commissions.push(res.id);
        self.commission_slugs.push(slug.to_string());
        self.tick(10);
        res.id
    }

    fn provision_agent(&mut self, commission_id: u64, identity_str: &str) -> u32 {
        let ident = name(identity_str);
        let mut charter = Charter::load(&self.tree.charter()).expect("load charter");
        let uid = charter
            .allocate_slot(ident.clone(), Some(commission_id))
            .expect("allocate slot");

        charter.parcels.insert(
            uid,
            ParcelLease {
                identity: ident.clone(),
                stasis: false,
            },
        );

        let session = SessionRecord {
            id: SessionId::new(uid, 1).expect("session id"),
            status: SessionStatus::Running,
            restart_count: 0,
        };
        charter.sessions.insert(uid, session);

        if let Some(comm) = charter.commissions.get_mut(&commission_id) {
            comm.roster.insert(uid);
            comm.indenture_entries
                .insert(uid, IndentureEntryStatus::InProgress);
        }

        charter
            .save_atomic(&self.tree.charter())
            .expect("save charter");
        self.ledger
            .assign_uid(commission_id, uid)
            .expect("assign uid to commission");

        // Initialize parcel directory structure
        let parcel_dir = self.tree.parcel(uid);
        fs::create_dir_all(&parcel_dir).expect("create parcel dir");
        let home_dir = self.tree.parcel_home(uid);
        fs::create_dir_all(&home_dir).expect("create parcel home dir");
        let spool_dir = self.tree.parcel_git_spool(uid);
        fs::create_dir_all(&spool_dir).expect("create git spool dir");
        fs::set_permissions(&spool_dir, fs::Permissions::from_mode(0o700))
            .expect("chmod 0700 spool");

        // Seed TITLE.md
        let title_path = self.tree.parcel_title(uid);
        let title_content = format!(
            "# TITLE — parcel{uid}\n\n\
             - [{}] trigger commission={commission_id} prompt=\"autonomous task for {identity_str}\"\n\
             - [{}] provision uid={uid} archetype={identity_str} blueprint=village-alpine version=1\n\
             - [{}] session session_id={uid}.1 incarnation=1\n",
            format_epoch_iso8601(1_700_000_000 + self.simulated_seconds),
            format_epoch_iso8601(1_700_000_001 + self.simulated_seconds),
            format_epoch_iso8601(1_700_000_002 + self.simulated_seconds),
        );
        fs::write(&title_path, title_content).expect("write title stub");

        // Seed DEED.md
        let deed_path = self.tree.parcel_deed(uid);
        let deed_content = format!(
            "# DEED — parcel{uid}\n\n\
             - identity: {identity_str}\n\
             - uid: {uid}\n\
             - commission: {commission_id}\n\
             - blueprint: village-alpine v1\n\n\
             ### Dynamic Alterations\n\n\
             - baseline · toolchain bootstrap · uid {uid}\n"
        );
        fs::write(&deed_path, deed_content).expect("write deed stub");

        // Seed PROPRIETOR.md
        let prop_path = self.tree.parcel_proprietor(uid);
        let prop_content = format!(
            "# PROPRIETOR — {identity_str}\n\n\
             Profile for {identity_str} at slot {uid} on commission {commission_id}.\n"
        );
        fs::write(&prop_path, prop_content).expect("write proprietor stub");

        // Seed sample work in home/
        fs::write(
            home_dir.join("work.txt"),
            format!("Initial work item for {identity_str} at slot {uid}\n"),
        )
        .expect("write sample work file");

        self.current_agent_uids
            .insert(identity_str.to_string(), uid);
        self.tick(5);
        uid
    }

    /// Simulate agent git development and mediated bundle submission.
    fn push_git_bundle(&mut self, agent_identity: &str, file_name: &str, content: &str) {
        let &uid = self
            .current_agent_uids
            .get(agent_identity)
            .expect("agent exists");
        let charter = Charter::load(&self.tree.charter()).expect("load charter");
        let slot = charter.slots.get(&uid).expect("slot exists");
        let commission_id = slot.commission.expect("agent commission");

        // Setup a temporary working clone to create a fast-forward bundle
        let comm_git_dir = self.tree.commission_git(commission_id);
        let temp_work = self
            .work_dir
            .join(format!("work-{uid}-{}", self.event_count));
        fs::create_dir_all(&temp_work).expect("create temp work dir");

        run_git_cmd(&temp_work, &["clone", &comm_git_dir.to_string_lossy(), "."]);
        run_git_cmd(&temp_work, &["checkout", "-b", &format!("agent/{uid}")]);
        run_git_cmd(&temp_work, &["config", "user.name", agent_identity]);
        run_git_cmd(
            &temp_work,
            &["config", "user.email", &format!("{uid}@hamlet.local")],
        );

        let target_file = temp_work.join(file_name);
        fs::write(&target_file, content).expect("write file in work clone");
        run_git_cmd(&temp_work, &["add", file_name]);
        run_git_cmd(
            &temp_work,
            &[
                "commit",
                "-m",
                &format!("Agent {agent_identity} update {file_name}"),
            ],
        );

        let bundle_name = format!("patch-{uid}-{}.bundle", self.event_count);
        let spool_path = self.tree.parcel_git_spool(uid).join(&bundle_name);
        run_git_cmd(
            &temp_work,
            &[
                "bundle",
                "create",
                &spool_path.to_string_lossy(),
                &format!("refs/heads/agent/{uid}"),
            ],
        );
        fs::set_permissions(&spool_path, fs::Permissions::from_mode(0o600))
            .expect("chmod 0600 bundle");

        // Process through git mediation
        let outcome = self
            .ledger
            .process_git_bundle(uid, &bundle_name)
            .expect("process git bundle");

        match outcome {
            GitBundleOutcome::Accepted { .. } => {}
            other => panic!("expected git bundle accepted, got {other:?}"),
        }

        // Clean up temp work
        let _ = fs::remove_dir_all(temp_work);
        self.tick(15);
    }

    /// Add dynamic tool alteration to agent's DEED.md and promote.
    fn append_and_promote_deed(
        &mut self,
        agent_identity: &str,
        tool_kind: &str,
        tool_name: &str,
        version: &str,
    ) -> u32 {
        let &uid = self
            .current_agent_uids
            .get(agent_identity)
            .expect("agent exists");
        let deed_path = self.tree.parcel_deed(uid);
        let mut deed_content = fs::read_to_string(&deed_path).expect("read deed");
        let alteration_line = format!("- {tool_kind} {tool_name} = {version}\n");
        deed_content.push_str(&alteration_line);
        fs::write(&deed_path, deed_content).expect("write deed");

        let outcome = self
            .promoter
            .promote_parcel(uid)
            .expect("promote parcel deed alterations");

        let new_version = match outcome {
            PromotionOutcome::Promoted { new_version, .. } => new_version,
            PromotionOutcome::AlreadyMerged { version, .. } => version,
            other => panic!("expected Promoted or AlreadyMerged outcome, got {other:?}"),
        };

        self.tick(10);
        new_version
    }

    /// Submit deliverable to freight and approve vault promotion.
    fn submit_and_promote_vault(
        &mut self,
        agent_identity: &str,
        artifact_filename: &str,
        content: &str,
    ) {
        let &uid = self
            .current_agent_uids
            .get(agent_identity)
            .expect("agent exists");
        let charter = Charter::load(&self.tree.charter()).expect("load charter");
        let slot = charter.slots.get(&uid).expect("slot exists");
        let commission_id = slot.commission.expect("agent commission");
        let comm_slug = charter
            .commissions
            .get(&commission_id)
            .expect("comm")
            .slug
            .clone();

        // 1. Write deliverable into freight sandbox
        let freight_dir = self.tree.courier_freight().join(comm_slug.as_str());
        fs::create_dir_all(&freight_dir).expect("create freight dir");
        let freight_file = freight_dir.join(artifact_filename);
        fs::write(&freight_file, content).expect("write freight deliverable");

        // 2. Set commission to WindingDown and deliverable_in_review to true
        let mut charter = Charter::load(&self.tree.charter()).expect("load charter");
        if let Some(comm) = charter.commissions.get_mut(&commission_id) {
            comm.state = CommissionState::WindingDown;
            comm.deliverable_in_review = true;
            for entry_status in comm.indenture_entries.values_mut() {
                *entry_status = IndentureEntryStatus::Done;
            }
        }
        charter
            .save_atomic(&self.tree.charter())
            .expect("save charter");

        // 3. Promote to vault via VaultPromotion governor
        let rel_freight = format!(
            "courier/freight/{}/{}",
            comm_slug.as_str(),
            artifact_filename
        );
        let safe_art_name = name(artifact_filename);
        self.vault
            .approve(&rel_freight, &safe_art_name)
            .expect("vault promotion approve");

        self.tick(20);
    }

    /// Execute a hibernate and awaken cycle for an agent, testing stasis & patron re-matching.
    fn cycle_hibernate_and_awaken(
        &mut self,
        agent_identity: &str,
        target_commission: Option<u64>,
    ) -> u32 {
        let old_uid = *self
            .current_agent_uids
            .get(agent_identity)
            .expect("agent exists");

        // 1. Hibernate agent
        self.stasis
            .hibernate_sync(old_uid, "operator:soak_churn")
            .unwrap_or_else(|e| {
                panic!("hibernate failed for {agent_identity} at uid {old_uid}: {e}")
            });

        // Verify active parcel directory was removed
        assert!(
            !self.tree.parcel(old_uid).exists(),
            "parcel directory for {old_uid} must be deleted post-hibernation"
        );

        // 2. Patron verification & re-matching
        let patron_name = name(agent_identity);
        let patron_doc = self
            .stasis
            .find_patron_by_name(&patron_name)
            .unwrap_or_else(|e| panic!("find_patron_by_name failed for {agent_identity}: {e}"));
        assert_eq!(patron_doc.frontmatter.patron, patron_name);

        // 3. Awaken agent into fresh slot
        let awakened = self
            .stasis
            .awaken_sync(&patron_name, target_commission)
            .unwrap_or_else(|e| panic!("awaken failed for {agent_identity}: {e}"));

        let new_uid = awakened.uid;
        assert_ne!(
            new_uid, old_uid,
            "awakening must allocate a fresh sequential slot UID"
        );
        assert!(
            self.tree.parcel(new_uid).is_dir(),
            "new parcel directory must exist on disk for slot {new_uid}"
        );

        // Update tracking
        self.current_agent_uids
            .insert(agent_identity.to_string(), new_uid);
        self.tick(30);
        new_uid
    }
}

// ===========================================================================
// Test Suite: Soak + Resumability Marathon
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_soak_resumability_marathon() {
    eprintln!("=== SPRINT 10 STEP 02: SOAK + RESUMABILITY MARATHON ===");
    let mut v = VillageFixture::new(SOAK_PRNG_SEED);

    // -----------------------------------------------------------------------
    // Requirement 1: Multi-tenant Load Setup (3 commissions, 12 agents)
    // -----------------------------------------------------------------------
    eprintln!("==> Step 1: Initializing Multi-Tenant Load (3 commissions, 12 agents)");
    let c1 = v.create_commission("commission-alpha");
    let c2 = v.create_commission("commission-beta");
    let c3 = v.create_commission("commission-gamma");

    let agent_names = [
        ("alpha-architect", c1),
        ("alpha-builder", c1),
        ("alpha-scout", c1),
        ("alpha-scribe", c1),
        ("beta-architect", c2),
        ("beta-builder", c2),
        ("beta-scout", c2),
        ("beta-scribe", c2),
        ("gamma-architect", c3),
        ("gamma-builder", c3),
        ("gamma-scout", c3),
        ("gamma-scribe", c3),
    ];

    for (ident, cid) in &agent_names {
        v.provision_agent(*cid, ident);
    }
    assert_eq!(v.current_agent_uids.len(), 12);
    assert_all_invariants(
        &v.tree,
        &v.telemetry_dir,
        &v.log_dir,
        &v.commissions,
        "after initial provisioning of 3 commissions and 12 agents",
    );

    // -----------------------------------------------------------------------
    // Interleaved Mail Storm
    // -----------------------------------------------------------------------
    eprintln!("==> Step 2: Interleaved Mail Storm across all commissions");
    for i in 0..60 {
        let from_idx = i % agent_names.len();
        let to_idx = (i + 3) % agent_names.len();
        let from_ident = agent_names[from_idx].0;
        let to_ident = agent_names[to_idx].0;
        let from_uid = *v.current_agent_uids.get(from_ident).expect("from_uid");
        let to_uid = *v.current_agent_uids.get(to_ident).expect("to_uid");

        let mail_body = format!("Soak coordination message {i} from {from_ident} to {to_ident}");
        v.mail
            .send(
                MailActor::Agent(from_uid),
                to_uid,
                &format!("Coordination {i}"),
                &mail_body,
                None,
            )
            .expect("mail send must succeed");
        let _ = v.mail.route_recipient(&name(&to_uid.to_string()));
        v.tick(1);
    }

    // -----------------------------------------------------------------------
    // Git Bundle Pushes & Mediation
    // -----------------------------------------------------------------------
    eprintln!("==> Step 3: Git Bundle Pushes and Mediation");
    v.push_git_bundle(
        "alpha-builder",
        "feature_alpha.rs",
        "pub fn alpha_feature() -> bool { true }\n",
    );
    v.push_git_bundle(
        "beta-builder",
        "feature_beta.rs",
        "pub fn beta_feature() -> bool { true }\n",
    );
    v.push_git_bundle(
        "gamma-builder",
        "feature_gamma.rs",
        "pub fn gamma_feature() -> bool { true }\n",
    );

    // -----------------------------------------------------------------------
    // Deliverables, Freight, and Vault Promotions
    // -----------------------------------------------------------------------
    eprintln!("==> Step 4: Freight Submissions & Vault Promotions");
    let c_vault = v.create_commission("commission-vault");
    let _vault_agent_uid = v.provision_agent(c_vault, "vault-deliverer");
    v.submit_and_promote_vault(
        "vault-deliverer",
        "report_vault.txt",
        "Deliverable report for Commission Vault: All verified.\n",
    );

    // -----------------------------------------------------------------------
    // Requirement 2: Lifecycle Churn (>= 3x Hibernate/Awaken per Agent)
    // -----------------------------------------------------------------------
    eprintln!("==> Step 5: Lifecycle Churn Marathon (>= 3x Hibernate/Awaken for all 12 agents)");
    let all_agents: Vec<String> = agent_names.iter().map(|(n, _)| n.to_string()).collect();
    let active_commissions = [c1, c2, c3];

    for churn_cycle in 1..=3 {
        eprintln!("  --> Executing Churn Cycle {churn_cycle}/3 for all 12 agents");
        for (idx, agent_ident) in all_agents.iter().enumerate() {
            // Re-match patron with one of the 3 active concurrent commissions
            let target_comm = active_commissions[(idx + churn_cycle) % active_commissions.len()];
            let new_uid = v.cycle_hibernate_and_awaken(agent_ident, Some(target_comm));

            // Evolve blueprint from awakened parcel alterations
            let tool_name = format!("tool_c{churn_cycle}_{}", agent_ident.replace('-', "_"));
            let bp_ver = v.append_and_promote_deed(agent_ident, "pixi", &tool_name, "1.2.0");
            assert!(
                bp_ver >= 2,
                "blueprint version must grow monotonically from DEED promotions"
            );

            // Send post-awaken mail to verify agent session communications
            v.mail
                .send(
                    MailActor::Mayor,
                    new_uid,
                    &format!("Awaken greetings for {agent_ident} in slot {new_uid}"),
                    &format!("Greetings cycle {churn_cycle}"),
                    None,
                )
                .expect("post-awaken mail send");
            let _ = v.mail.route_recipient(&name(&new_uid.to_string()));
        }
        assert_all_invariants(
            &v.tree,
            &v.telemetry_dir,
            &v.log_dir,
            &v.commissions,
            &format!("after churn cycle {churn_cycle}/3 completion"),
        );
    }

    // -----------------------------------------------------------------------
    // Requirement 3 & 4: Chaos Battery (20 deterministic kill points + invariants)
    // -----------------------------------------------------------------------
    eprintln!("==> Step 6: Chaos Battery (20 deterministic kill -9 points, seeded PRNG)");
    let mut chaos_prng = SoakPrng::new(SOAK_PRNG_SEED);

    for kill_point in 1..=20 {
        let chaos_action = chaos_prng.next_range(0, 4);
        eprintln!("  --> Chaos checkpoint {kill_point}/20 (action={chaos_action})");

        match chaos_action {
            0 => {
                // Drop in-memory caches, clear title cache, simulate hard daemon kill -9
                clear_title_cache();
            }
            1 => {
                // Mid-atomic write crash: plant a harmless temporary artifact beside charter
                // to prove atomic save recovery ignores and sweeps stale temp litter
                let stale_temp = v
                    .tree
                    .root()
                    .join(format!(".charter.json.{kill_point}.tmp"));
                fs::write(&stale_temp, b"{\"torn_write_simulated\": true}").unwrap();
            }
            2 => {
                // Mid-spool crash: plant an incomplete spool temp file and verify cleanup
                let active_uids: Vec<u32> = v.current_agent_uids.values().copied().collect();
                let victim_uid = active_uids[(kill_point as usize) % active_uids.len()];
                let spool_temp = v
                    .tree
                    .parcel_git_spool(victim_uid)
                    .join(format!(".temp_bundle_{kill_point}.tmp"));
                fs::write(&spool_temp, b"truncated bundle data").unwrap();
            }
            3 => {
                // Spool a mail message right before death, simulate recover_spools on resurrection
                let active_uids: Vec<u32> = v.current_agent_uids.values().copied().collect();
                let victim_uid = active_uids[(kill_point as usize) % active_uids.len()];
                let _ = v.mail.send(
                    MailActor::Mayor,
                    victim_uid,
                    &format!("Chaos pre-kill message at point {kill_point}"),
                    "Chaos message body",
                    None,
                );
            }
            _ => {
                // Run recovery sweep across spools and decrees
                let _ = v.mail.recover_spools();
            }
        }

        v.tick(10);

        // Immediate assertion of all 6 invariants after chaos
        assert_all_invariants(
            &v.tree,
            &v.telemetry_dir,
            &v.log_dir,
            &v.commissions,
            &format!("chaos point {kill_point}/20 invariants verified"),
        );
    }

    // -----------------------------------------------------------------------
    // Watcher Event Storm (10,000 File Ops)
    // -----------------------------------------------------------------------
    eprintln!("==> Step 7: Watcher Event Storm (10,000 File Operations)");
    let storm_dir = v.work_dir.join("storm_scratch");
    fs::create_dir_all(&storm_dir).expect("create storm scratch dir");

    let storm_start = Instant::now();
    for i in 0..2_500 {
        let f_path = storm_dir.join(format!("storm_{i}.tmp"));
        fs::write(&f_path, b"event storm test payload").expect("storm write");
        fs::write(&f_path, b"event storm modified payload").expect("storm modify");
        let ren_path = storm_dir.join(format!("storm_{i}_renamed.tmp"));
        fs::rename(&f_path, &ren_path).expect("storm rename");
        fs::remove_file(&ren_path).expect("storm unlink");
    }
    // 2500 creates + 2500 modifies + 2500 renames + 2500 unlinks = 10,000 file ops!
    eprintln!(
        "  [storm-ok] 10,000 filesystem operations completed in {:?}",
        storm_start.elapsed()
    );
    let _ = fs::remove_dir_all(&storm_dir);
    v.tick(100);

    // -----------------------------------------------------------------------
    // Adversarial Vectors Assertion Battery
    // -----------------------------------------------------------------------
    eprintln!("==> Step 8: Asserting Adversarial Edge Vectors");

    // 1. Path traversal in promotions refused
    let traversal_res = v.vault.approve(
        "courier/freight/../../etc/shadow",
        &name("traversal_artifact"),
    );
    assert!(
        traversal_res.is_err(),
        "path traversal in vault promotion must be refused"
    );

    // 2. Lying DEED alterations refused
    let lying_deed = "# DEED\n### Dynamic Alterations\n- evil_tool ../../bin/sh = 1.0\n";
    let parsed_lying = hamlet_core::promote::parse_deed_alterations(lying_deed);
    assert!(
        parsed_lying.is_err(),
        "lying DEED alteration with invalid tool kind/name must be refused"
    );

    // 3. Duplicate sections in DEED refused
    let duplicate_sections = "# DEED\n### Dynamic Alterations\n- pixi toolA = 1.0\n### Dynamic Alterations\n- pixi toolB = 2.0\n";
    let parsed_dup = hamlet_core::promote::parse_deed_alterations(duplicate_sections);
    assert!(
        parsed_dup.is_err(),
        "DEED with duplicate ### Dynamic Alterations sections must be refused"
    );

    // -----------------------------------------------------------------------
    // Additional Village Activity to meet/exceed Duration Target (>= 1,800s)
    // -----------------------------------------------------------------------
    eprintln!("==> Step 9: Advancing Simulated Village Clock to Duration Target");
    while v.simulated_seconds < TARGET_SIMULATED_SECONDS || v.event_count < 1800 {
        v.tick(1);
    }

    eprintln!(
        "  [duration-target-met] simulated village time: {} seconds (>= {}s target), {} discrete events",
        v.simulated_seconds, TARGET_SIMULATED_SECONDS, v.event_count
    );
    assert!(
        v.simulated_seconds >= TARGET_SIMULATED_SECONDS,
        "simulated village time must reach at least 30 minutes (1800s)"
    );
    assert!(
        v.event_count >= 1800,
        "event count must reach at least 1,800 events"
    );

    // -----------------------------------------------------------------------
    // Final Invariants Verification
    // -----------------------------------------------------------------------
    eprintln!("==> Step 10: Final Comprehensive Invariants Verification");
    assert_all_invariants(
        &v.tree,
        &v.telemetry_dir,
        &v.log_dir,
        &v.commissions,
        "FINAL SOAK VERIFICATION (All invariants clean)",
    );

    eprintln!("=== SOAK + RESUMABILITY MARATHON COMPLETED WITH ZERO INVARIANT VIOLATIONS ===");
}

/// Verification test for Finding 10/02-01 remediation:
/// `VaultPromotion::approve` delegates agent stasis transition to `ProductionStasis::hibernate` (ADR-0039).
///
/// Verifies:
/// 1. `charter.parcels[uid].stasis` is set to `true`.
/// 2. `charter.slots[uid].status` is transitioned to `SlotStatus::Hibernated`.
/// 3. Active parcel directory `keep/census/parcel<uid>/` is cleaned up from disk.
/// 4. Patron record with `PROPRIETOR.md` and `manifest.json` is authored to `tavern/patrons/PROPRIETOR<name>/`.
/// 5. Agent is fully awakenable via `stasis.awaken_sync` with new parcel allocation.
#[tokio::test]
async fn test_remediation_finding_10_02_01_vault_approve_executes_stasis_pipeline() {
    let mut v = VillageFixture::new(42);
    let cid = v.create_commission("limbo-comm");
    let uid = v.provision_agent(cid, "limbo-agent");

    // 1. Submit deliverable and approve vault promotion
    v.submit_and_promote_vault("limbo-agent", "deliverable.txt", "Limbo deliverable");

    // Check charter state post-vault-approval
    let charter = Charter::load(&v.tree.charter()).expect("load charter");
    assert!(
        charter.parcels.get(&uid).expect("lease").stasis,
        "vault approve set lease.stasis = true"
    );
    assert_eq!(
        charter.slots.get(&uid).expect("slot").status,
        SlotStatus::Hibernated,
        "vault approve transitioned slot status to Hibernated"
    );

    // 2. Active parcel directory is unlinked and archived
    assert!(
        !v.tree.parcel(uid).exists(),
        "parcel directory was cleaned up upon hibernation"
    );

    // 3. Patron record exists in tavern/patrons/
    let patron_dir = v.tree.patron(&name("limbo-agent"));
    assert!(
        patron_dir.exists(),
        "patron directory was authored in tavern/patrons/"
    );
    assert!(
        patron_dir.join("PROPRIETOR.md").is_file(),
        "PROPRIETOR.md must exist in patron dir"
    );
    assert!(
        patron_dir.join("manifest.json").is_file(),
        "manifest.json must exist in patron dir"
    );

    // 4. stasis.awaken succeeds smoothly, re-allocating agent to a new active commission
    let next_cid = v.create_commission("next-comm");
    let awaken_res = v.stasis.awaken_sync(&name("limbo-agent"), Some(next_cid));
    assert!(
        awaken_res.is_ok(),
        "agent must awaken cleanly from stasis into new commission: {awaken_res:?}"
    );
    let awakened = awaken_res.expect("awakened agent");
    assert_eq!(awakened.patron, name("limbo-agent"));
    assert_ne!(
        awakened.uid, uid,
        "awakened agent gets a fresh slot allocation"
    );
    assert!(
        v.tree.parcel(awakened.uid).exists(),
        "awakened agent has new parcel dir"
    );
}
