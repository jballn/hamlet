//! QA 02/05 — adversarial isolation & escape suite (FakeRuntime level).
//!
//! Sprint 02 step 05. Extends (never duplicates) `provision.rs`,
//! `bootstrap.rs` and the guarded podman suites with the QA-mandated
//! adversarial angles:
//!
//! 1. **ro mounts / no infrastructure in argv** — a FULL provisioning flow's
//!    recorded argv is audited for privilege escalation, host-namespace
//!    flags, infrastructure sockets and any `sh -c`; the `run` argv's `-v`
//!    rows must be exactly the SPEC §4 table with `/var/git/` + `/var/
//!    var/context/` ro and every host path inside the village tree.
//! 2. **Userns/chown policy** — every `podman unshare chown` target is a
//!    slot uid or the rollback reclaim (0:0); shared/ro mounts are NEVER
//!    chowned away from the daemon (host-side braces, ADR-0011 cl. 3).
//! 3. **Provisioning rollback at EVERY step** — failure injection into
//!    `unshare`/`image`/`pull`/`build`/`run`/`exec`: no orphan dirs, no
//!    orphan container, charter.json byte-identical, retry re-uses the same
//!    slot (no slot leak).
//! 4. **Remnant-parcel adoption** — documents the orphan-stub behaviour
//!    inside a PRE-EXISTING parcel dir after a mid-provision failure
//!    (QA finding F-02-01, HANDOFF sprint 02).
//! 5. **Slot exhaustion** — provisions a REAL 1000 agents through the
//!    provisioner: sequential 1001..=2000, the 1001st is a hard error with
//!    zero side effects, existing identities stay idempotent, no wrap.
//! 6. **Bootstrap integrity** — `.env` secret material never reaches any
//!    argv, any exec stdin payload, or any persisted file in the tree.
//!
//! SPEC citations: §4 (isolation invariants, UID policy), §11.4 (secrets);
//! CONVENTIONS §3 r5 (slot exhaustion hard error), §5 (no privileged mode,
//! ro mounts), §9 (mock-seam testing doctrine).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::pins::ToolchainPins;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::{AGENT_SLOT_CAPACITY, FIRST_AGENT_UID, LAST_AGENT_UID};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::script_pinned_digests;
use hamlet_daemon::census::{ProvisionError, Provisioner};
use hamlet_daemon::podman::{FakeRuntime, PodmanErrorKind};

const BRIEF: &str = "# scribe\n\nCore principles: copy faithfully, annotate sparingly.";

/// Obvious fake (CONVENTIONS §6) — proves `.env` never rides along.
const FAKE_SECRET: &str = "test-key-do-not-use-QA02-x9";

const TEST_MANIFEST_SHA256: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("test name must sanitize")
}

fn scripted(uids: &[u32]) -> Arc<FakeRuntime> {
    let runtime = Arc::new(FakeRuntime::new());
    let pins = ToolchainPins::default_hamlet().unwrap();
    for uid in uids {
        script_pinned_digests(&runtime, *uid, &pins, TEST_MANIFEST_SHA256);
    }
    runtime
}

struct Fixture {
    tree: HamletTree,
    runtime: Arc<FakeRuntime>,
    provisioner: Provisioner,
    blueprint: Blueprint,
    _tmp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    Charter::empty().save_atomic(&tree.charter()).unwrap();
    fs::write(tree.register_brief(&name("scribe")), BRIEF).unwrap();
    fs::write(tree.register_brief(&name("quill")), BRIEF).unwrap();
    let runtime = scripted(&[1001, 1002]);
    let provisioner = Provisioner::new(
        tree.clone(),
        Config::default(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    Fixture {
        tree,
        runtime,
        provisioner,
        blueprint: Blueprint::default_village_alpine().unwrap(),
        _tmp: tmp,
    }
}

/// Substrings that must never appear in ANY recorded argv of a full
/// provisioning flow (SPEC §4: rootless, no privileged mode, no host
/// infrastructure; ADR-0010 cl. 4 forbidden-flag list, extended by QA with
/// dbus / cgroup / generic-socket / remote-API surfaces).
const FORBIDDEN_ARGV_SUBSTRINGS: &[&str] = &[
    "--privileged",
    "--userns", // ADR-0011: default rootless userns, never an explicit flag
    "--pid=host",
    "--network=host",
    "--ipc=host",
    "--authfile",
    "--remote",
    "--url",
    "podman.sock",
    "docker.sock",
    ".sock",
    "dbus",
    ":/run",
    ":/var/run",
    ":/sys",
    ":/proc",
    ":/dev",
    ":/etc",
    "sh -c",
];

fn host_paths_of_volumes(argv: &[String]) -> Vec<(String, String, bool)> {
    let mut rows = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if a == "-v" {
            let value = argv.get(i + 1).cloned().unwrap_or_default();
            let ro = value.ends_with(":ro");
            let trimmed = value.strip_suffix(":ro").unwrap_or(&value);
            let (host, container) = trimmed.split_once(':').unwrap_or((trimmed, ""));
            rows.push((host.to_owned(), container.to_owned(), ro));
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// 1 + 3. Full-flow argv audit: privilege, infrastructure, ro rendering,
//       mount-table closure, bootstrap containment.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_provision_argv_is_free_of_privilege_and_host_infrastructure() {
    let f = fixture();
    f.provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();

    let log = f.runtime.argv_log();
    assert!(
        log.len() > 20,
        "a full provision records the whole flow (unshare/image/run/exec…): {}",
        log.len()
    );
    let root = f.tree.root().to_str().unwrap().to_owned();

    for argv in &log {
        assert_eq!(argv[0], "podman", "argv[0] is always the binary");
        let joined = argv.join(" ");
        for forbidden in FORBIDDEN_ARGV_SUBSTRINGS {
            assert!(
                !joined.contains(forbidden),
                "QA audit: forbidden {forbidden:?} in recorded argv: {joined}"
            );
        }
        // Every host path referenced by a volume flag lives in the tree.
        for (host, _container, _ro) in host_paths_of_volumes(argv) {
            assert!(
                host.starts_with(&root),
                "volume host path escapes the village tree: {host}"
            );
        }
        // Bootstrap/exec containment: every absolute in-container path is
        // the slot home, the shared cache, or a declared read-only mount —
        // never another uid's home, never /etc, /usr, /bin, /var/git writes.
        if argv[1] == "exec" {
            for token in argv.iter() {
                if token.starts_with('/') {
                    let ok = token.starts_with("/home/1001")
                        || token.starts_with("/var/pkgcache")
                        || token == "/var/mail"
                        || token.starts_with("/var/mail/1001");
                    assert!(
                        ok,
                        "QA containment: exec argv touches foreign path {token}: {joined}"
                    );
                }
            }
        }
    }

    // The single run argv: hardening flags + EXACTLY the SPEC §4 table.
    let run = log.iter().find(|a| a[1] == "run").expect("run recorded");
    let runj = run.join(" ");
    for required in [
        "--security-opt=no-new-privileges",
        "--cap-drop=ALL",
        "--user 1001:1001",
        "--pids-limit",
        "--memory",
        "--cpus",
    ] {
        assert!(
            runj.contains(required),
            "run argv missing {required}: {runj}"
        );
    }
    let rows = host_paths_of_volumes(run);
    assert_eq!(rows.len(), 6, "exactly the SPEC §4 agent rows: {rows:?}");
    let by_container = |c: &str| rows.iter().find(|r| r.1 == c).cloned();
    let git = by_container("/var/git/").expect("/var/git/ mounted");
    let ctx = by_container("/var/context/").expect("/var/context/ mounted");
    assert!(git.2, "/var/git/ is ro (SPEC §4)");
    assert!(ctx.2, "/var/context/ is ro (SPEC §4)");
    for c in [
        "/home/1001/",
        "/var/mail/",
        "/var/freight/",
        "/var/pkgcache/",
    ] {
        let row = by_container(c).unwrap_or_else(|| panic!("{c} mounted"));
        assert!(!row.2, "{c} is rw per SPEC §4");
    }
    assert_eq!(git.0, f.tree.commission_git(7).to_string_lossy());
    assert_eq!(ctx.0, f.tree.tavern_gossip().to_string_lossy());
}

// ---------------------------------------------------------------------------
// 2 + 4. chown policy: slot uids (or reclaim 0:0) only; shared/ro mounts
//        never leave daemon ownership (the host-side "braces").
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unshare_chown_targets_slot_uids_only_and_never_shared_mounts() {
    let f = fixture();
    f.provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    // A failed second provision (different slot) forces the rollback reclaim
    // (chown 0:0) into the same log.
    f.runtime
        .fail_next_for("run", PodmanErrorKind::NonZeroExit { code: Some(125) });
    f.provisioner
        .provision_parcel(&name("quill"), 7, &f.blueprint)
        .await
        .unwrap_err();

    let root = f.tree.root().to_str().unwrap().to_owned();
    let shared_prefixes: Vec<String> = [
        f.tree.commission_git(7),
        f.tree.tavern_gossip(),
        f.tree.courier_freight(),
        f.tree.market_pkgcache(),
        f.tree.courier_mail(), // the ROOT — spool subdirs are slot-owned
    ]
    .map(|p| format!("{}/", p.to_string_lossy()))
    .to_vec();

    let mut saw_slot_chown = false;
    let mut saw_reclaim = false;
    for argv in f.runtime.argv_log() {
        if argv[1] != "unshare" {
            continue;
        }
        assert_eq!(argv[2], "chown", "unshare is only ever used for chown");
        let owner = &argv[3];
        assert_eq!(argv[4], "--", "paths are separated by --");
        let (uid, gid) = owner.split_once(':').expect("uid:gid");
        assert_eq!(uid, gid, "owner and group always match");
        let paths = &argv[5..];
        assert!(!paths.is_empty(), "chown always names paths");
        match owner.as_str() {
            "0:0" => {
                saw_reclaim = true;
                // Rollback reclaim only: exactly the paths a failed run
                // handed to a slot (home + spool), nothing else.
                for p in paths {
                    let is_home = p.starts_with(&format!("{root}/keep/census/parcel1002/home"));
                    let is_spool = p.ends_with("/courier/mail/1002");
                    assert!(
                        is_home || is_spool,
                        "reclaim chown 0:0 touched foreign path {p}"
                    );
                }
            }
            "1001:1001" | "1002:1002" => {
                saw_slot_chown = true;
                for p in paths {
                    assert!(
                        p.starts_with(&root),
                        "slot chown escapes the village tree: {p}"
                    );
                    for shared in &shared_prefixes {
                        // The spool dir itself lives UNDER courier/mail/ —
                        // only the ROOT (exact path) is forbidden.
                        let exact_root = shared.strip_suffix('/').unwrap().to_owned();
                        assert_ne!(p, &exact_root, "shared mount root chowned to a slot");
                        if p.starts_with(shared) && !p.contains("/courier/mail/") {
                            panic!("shared/ro mount content chowned to a slot: {p}");
                        }
                    }
                }
            }
            other => panic!(
                "QA policy: chown target {other} is neither a slot uid nor the 0:0 reclaim \
                 (ADR-0011: daemon host user must never appear; slot uids only)"
            ),
        }
    }
    assert!(saw_slot_chown, "provisioning chowns home+spool to the slot");
    assert!(saw_reclaim, "rollback reclaims via chown 0:0");

    // Host-side braces on the REAL fs (modes the fake chown cannot touch):
    // ro/shared mounts stay daemon-owned 0755/01770 → a mapped (sub)uid has
    // no write bit. Belt (ro mount) AND braces (perms), SPEC §4.
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&f.tree.commission_git(7)), 0o755);
    assert_eq!(mode(&f.tree.tavern_gossip()), 0o755);
    assert_eq!(mode(&f.tree.courier_freight()), 0o1770);
    assert_eq!(mode(&f.tree.courier_mail()), 0o755);
    assert_eq!(mode(&f.tree.market_pkgcache()), 0o1777);
    assert_eq!(mode(&f.tree.parcel_home(1001)), 0o700);
    assert_eq!(mode(&f.tree.courier_mail_recipient(&name("1001"))), 0o700);
}

// ---------------------------------------------------------------------------
// 3. Rollback at EVERY provision step (task item 5).
// ---------------------------------------------------------------------------

struct Injection {
    /// Human-readable step name (assertion messages).
    step: &'static str,
    /// Verb to fail (`fail_next_for`), or None for the charter-save squat.
    verb: Option<&'static str>,
    /// Derived-image probe must miss so pull/build happen at all.
    image_missing: bool,
    /// Whether the container was started before the injected failure (so a
    /// `rm --force` MUST appear in the rollback).
    container_started: bool,
}

#[tokio::test]
async fn failure_at_every_provision_step_leaves_no_orphan_and_charter_untouched() {
    let injections = [
        Injection {
            step: "host permissions (first unshare chown)",
            verb: Some("unshare"),
            image_missing: false,
            container_started: false,
        },
        Injection {
            step: "derived-image probe (podman image exists)",
            verb: Some("image"),
            image_missing: false,
            container_started: false,
        },
        Injection {
            step: "parent-image pull",
            verb: Some("pull"),
            image_missing: true,
            container_started: false,
        },
        Injection {
            step: "derived-image build",
            verb: Some("build"),
            image_missing: true,
            container_started: false,
        },
        Injection {
            step: "container start (podman run)",
            verb: Some("run"),
            image_missing: false,
            container_started: false,
        },
        Injection {
            step: "toolchain bootstrap (first exec inside the container)",
            verb: Some("exec"),
            image_missing: false,
            container_started: true,
        },
    ];

    for inj in injections {
        let f = fixture();
        let before = fs::read(f.tree.charter()).unwrap();
        if inj.image_missing {
            f.runtime.set_image_exists(false);
        }
        if let Some(verb) = inj.verb {
            f.runtime
                .fail_next_for(verb, PodmanErrorKind::NonZeroExit { code: Some(1) });
        }

        let err = f
            .provisioner
            .provision_parcel(&name("scribe"), 7, &f.blueprint)
            .await
            .unwrap_err();
        assert!(
            !matches!(err, ProvisionError::RollbackFailed { .. }),
            "[{}] rollback must complete cleanly, got: {err}",
            inj.step
        );

        // No orphan dirs anywhere this run could have created.
        assert!(
            !f.tree.parcel(1001).exists(),
            "[{}] parcel dir orphaned",
            inj.step
        );
        assert!(
            !f.tree.courier_mail_recipient(&name("1001")).exists(),
            "[{}] spool orphaned",
            inj.step
        );
        assert!(
            !f.tree.commission(7).exists(),
            "[{}] commission dir orphaned",
            inj.step
        );

        // Charter byte-identical: no slot, no lease, no commission on disk.
        let after = fs::read(f.tree.charter()).unwrap();
        assert_eq!(
            before, after,
            "[{}] charter.json changed after failed provision",
            inj.step
        );

        // No orphan container: either run was never attempted, or the
        // rollback force-removed it.
        let log = f.runtime.argv_log();
        let ran = log.iter().any(|a| a[1] == "run");
        let removed = log
            .iter()
            .any(|a| a == &["podman", "rm", "--force", "scribe"]);
        if inj.container_started {
            assert!(
                ran && removed,
                "[{}] started container not removed",
                inj.step
            );
        } else if !ran {
            assert!(!removed, "[{}] rm without run?", inj.step);
        }

        // No slot leak: the retry (injection consumed) re-uses slot 1001.
        if inj.image_missing {
            f.runtime.set_image_exists(true);
        }
        let lease_uid_dir = f.tree.parcel(1001);
        f.provisioner
            .provision_parcel(&name("scribe"), 7, &f.blueprint)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "[{}] retry after clean rollback must succeed: {e}",
                    inj.step
                )
            });
        assert!(
            lease_uid_dir.is_dir(),
            "[{}] retry re-uses slot 1001 (no leak, no wrap)",
            inj.step
        );
        let charter = Charter::load(&f.tree.charter()).unwrap();
        assert_eq!(charter.slots.len(), 1, "[{}] exactly one slot", inj.step);
        assert!(charter.slots.contains_key(&1001));
    }
}

// ---------------------------------------------------------------------------
// 4. Remnant parcel: orphan stub adoption (QA finding F-02-01).
// ---------------------------------------------------------------------------

/// Rollback tracks DIRECTORIES, not files: when the parcel dir pre-exists
/// (crash remnant) and the run fails after the stubs were written, the
/// stub documents created by THIS run survive inside the remnant dir.
/// This test pins the actual behaviour and proves it is non-destructive
/// and convergent: the retry adopts the stubs and completes. Filed as
/// F-02-01 (observation) in HANDOFF — task item 5 lists orphan
/// "slots/dirs/containers", and stubs are adopted-on-retry lineage, but
/// the deviation is recorded for the product owners.
#[tokio::test]
async fn failure_inside_remnant_parcel_leaves_adoptable_stubs_and_retry_converges() {
    let f = fixture();
    // Crash remnant: parcel dir exists, home/stubs do not.
    fs::create_dir_all(f.tree.parcel(1001)).unwrap();
    f.runtime
        .fail_next_for("run", PodmanErrorKind::NonZeroExit { code: Some(125) });
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::Podman(_)), "{err}");

    // Dirs created by THIS run are gone; the remnant parcel dir survives.
    assert!(f.tree.parcel(1001).is_dir(), "remnant preserved");
    assert!(!f.tree.parcel_home(1001).exists(), "home unwound");
    assert!(!f.tree.courier_mail_recipient(&name("1001")).exists());
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), Charter::empty());

    // F-02-01 (actual behaviour, pinned): THIS run's stubs remain.
    assert!(
        f.tree.parcel_title(1001).exists(),
        "pinned actual: TITLE.md created before the failure survives in the remnant dir"
    );

    // Convergence: the retry adopts the stubs (never clobbers) and succeeds.
    let title_before = fs::read_to_string(f.tree.parcel_title(1001)).unwrap();
    f.provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(f.tree.parcel_title(1001)).unwrap(),
        title_before,
        "adopted verbatim — lineage from the first attempt is preserved"
    );
    assert!(f.tree.parcel_home(1001).is_dir());
    assert!(Charter::load(&f.tree.charter())
        .unwrap()
        .parcels
        .contains_key(&1001));
}

// ---------------------------------------------------------------------------
// 5. Slot exhaustion: a REAL thousand-agent provision (task item 6).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provisioning_a_thousand_agents_exhausts_slots_without_wrap() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    Charter::empty().save_atomic(&tree.charter()).unwrap();
    let blueprint = Blueprint::default_village_alpine().unwrap();
    let pins = ToolchainPins::default_hamlet().unwrap();

    // Provision 1000 agents through the real flow (fresh FakeRuntime per
    // agent keeps the exec-script tables small; the tree/charter are shared).
    for i in 0..AGENT_SLOT_CAPACITY {
        let identity = name(&format!("agent-{i}"));
        fs::write(tree.register_brief(&identity), BRIEF).unwrap();
        let runtime = Arc::new(FakeRuntime::new());
        script_pinned_digests(
            &runtime,
            FIRST_AGENT_UID + i as u32,
            &pins,
            TEST_MANIFEST_SHA256,
        );
        let provisioner = Provisioner::new(
            tree.clone(),
            Config::default(),
            Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        );
        let commission = (i % 7) as u64 + 1;
        let lease = provisioner
            .provision_parcel(&identity, commission, &blueprint)
            .await
            .unwrap_or_else(|e| panic!("agent #{i} ({identity}): {e}"));
        assert!(!lease.stasis);
        // Sequential, permanent, no gaps (SPEC §4 / CONVENTIONS §3 r5) —
        // spot-checked to keep the loop off the O(n²) charter re-parse.
        if i % 250 == 249 || i == 0 {
            let charter = Charter::load(&tree.charter()).unwrap();
            assert_eq!(
                charter.agents[&identity].slot,
                FIRST_AGENT_UID + i as u32,
                "agent #{i} slot"
            );
            eprintln!("QA exhaustion: {}/1000 agents provisioned", i + 1);
        }
    }

    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.slots.len(), AGENT_SLOT_CAPACITY);
    for (idx, uid) in charter.slots.keys().enumerate() {
        assert_eq!(*uid, FIRST_AGENT_UID + idx as u32, "slots sorted, no gaps");
    }
    assert_eq!(*charter.slots.keys().next().unwrap(), FIRST_AGENT_UID);
    assert_eq!(*charter.slots.keys().next_back().unwrap(), LAST_AGENT_UID);
    // Full identity→slot mapping: sequential and permanent for all 1000.
    for i in 0..AGENT_SLOT_CAPACITY {
        let identity = name(&format!("agent-{i}"));
        assert_eq!(
            charter.agents[&identity].slot,
            FIRST_AGENT_UID + i as u32,
            "identity #{i} holds its sequential slot"
        );
        assert_eq!(
            charter.slots[&(FIRST_AGENT_UID + i as u32)].identity,
            identity
        );
    }

    // The 1001st: hard error, zero side effects, charter untouched.
    let before = fs::read(tree.charter()).unwrap();
    let identity = name("agent-1000");
    fs::write(tree.register_brief(&identity), BRIEF).unwrap();
    let runtime = Arc::new(FakeRuntime::new());
    let provisioner = Provisioner::new(
        tree.clone(),
        Config::default(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    let err = provisioner
        .provision_parcel(&identity, 3, &blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProvisionError::Slot(hamlet_core::slots::SlotError::Exhausted { identity: id }) if *id == identity),
        "exhaustion is a typed hard error (CONVENTIONS §3 r5): {err}"
    );
    assert!(
        runtime.argv_log().is_empty(),
        "exhausted provision fires ZERO podman calls"
    );
    assert!(
        !tree.parcel(LAST_AGENT_UID + 1).exists(),
        "no wrap to uid 2001"
    );
    assert_eq!(
        fs::read(tree.charter()).unwrap(),
        before,
        "charter unchanged"
    );

    // No wrap, no reuse: an EXISTING identity still re-provisions onto its
    // permanent slot (idempotent), while the charter never grows.
    script_pinned_digests(&runtime, FIRST_AGENT_UID, &pins, TEST_MANIFEST_SHA256);
    let provisioner = Provisioner::new(
        tree.clone(),
        Config::default(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    provisioner
        .provision_parcel(&name("agent-0"), 1, &blueprint)
        .await
        .expect("existing identities stay deployable at exhaustion");
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.agents[&name("agent-0")].slot, FIRST_AGENT_UID);
    assert_eq!(
        charter.slots.len(),
        AGENT_SLOT_CAPACITY,
        "never grows past 1000"
    );
}

// ---------------------------------------------------------------------------
// 6. Bootstrap integrity: .env secrets never ride along (task item 7).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn env_secret_material_never_reaches_argv_stdin_or_the_tree() {
    let f = fixture();
    // SPEC §11.4 / CONVENTIONS §6: obvious fake in the real .env location.
    fs::write(
        f.tree.env_file(),
        format!("HAMLET_QA_SECRET={FAKE_SECRET}\nOTHER=benign\n"),
    )
    .unwrap();

    f.provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();

    // (a) No recorded argv mentions the secret or the .env file.
    for argv in f.runtime.argv_log() {
        let joined = argv.join(" ");
        assert!(!joined.contains(FAKE_SECRET), "secret in argv: {joined}");
        assert!(
            !joined.contains(".env"),
            "provisioning never touches .env: {joined}"
        );
    }
    // (b) No exec stdin payload (the bytes that land in homes) carries it.
    for (argv, payload) in f.runtime.stdin_log() {
        let body = String::from_utf8_lossy(&payload);
        assert!(
            !body.contains(FAKE_SECRET),
            "secret in stdin to {}: {}",
            argv.join(" "),
            body
        );
        assert!(!body.contains(".env"), "stdin references .env: {body}");
    }
    // (c) No persisted file ANYWHERE in the village tree contains it —
    //     charter, TITLE/DEED/PROPRIETOR, config, pkgcache mirror, all.
    fn scan(dir: &Path, hits: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                scan(&path, hits);
            } else if meta.is_file() && path.file_name().unwrap() != ".env" {
                if let Ok(bytes) = fs::read(&path) {
                    if bytes
                        .windows(FAKE_SECRET.len())
                        .any(|w| w == FAKE_SECRET.as_bytes())
                    {
                        hits.push(path.display().to_string());
                    }
                }
            }
        }
    }
    let mut hits = Vec::new();
    scan(f.tree.root(), &mut hits);
    assert!(
        hits.is_empty(),
        "secret material persisted outside .env: {hits:?} (SPEC §11.4)"
    );
}
