//! Integration tests for the hibernation routine and patron stasis
//! (SPEC §10, ADR-0039, ADR-0040).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState};
use hamlet_core::config::Config;
use hamlet_core::decree::{Decree, DecreeId, DECREE_VERSION};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionRecord, SessionStatus};
use hamlet_core::slots::SlotStatus;
use hamlet_core::stasis::{BackupManifest, ManifestEntryType, PatronDoc, TitleDoc, TitleEntry};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::{clear_title_cache, list_census, ProductionStasis, StasisError};
use hamlet_daemon::decrees::{DecreeExecutor, Stasis};
use hamlet_daemon::podman::FakeRuntime;
use hamlet_daemon::telemetry::{AgentKey, Telemetry};

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid name")
}

fn write_decree(tree: &HamletTree, decree: &Decree) -> PathBuf {
    let fname = decree.id().decree_file_name().unwrap();
    let path = tree.manor_decrees().join(fname.as_str());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, decree.render().unwrap()).unwrap();
    path
}

fn setup_village(tmp: &tempfile::TempDir) -> (HamletTree, Arc<FakeRuntime>, Arc<Telemetry>) {
    clear_title_cache();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let runtime = Arc::new(FakeRuntime::new());
    let telemetry = Arc::new(Telemetry::open_for_village(tree.root(), &Config::default()).unwrap());

    // Initialize charter with slot 1001 (agent "scribe" on commission 42)
    let mut charter = Charter::empty();
    let identity = name("scribe");
    let slot_uid = charter.allocate_slot(identity.clone(), Some(42)).unwrap();
    assert_eq!(slot_uid, 1001);

    charter.parcels.insert(
        slot_uid,
        ParcelLease {
            identity: identity.clone(),
            stasis: false,
        },
    );

    let session = SessionRecord {
        id: SessionId::new(slot_uid, 1).unwrap(),
        status: SessionStatus::Running,
        restart_count: 0,
    };
    charter.sessions.insert(slot_uid, session);

    let mut comm = CommissionRecord::draft(name("transcription"));
    comm.state = CommissionState::Active;
    comm.roster.insert(slot_uid);
    charter.commissions.insert(42, comm);

    charter.save_atomic(&tree.charter()).unwrap();

    // Seed parcel files
    let parcel_dir = tree.parcel(slot_uid);
    fs::create_dir_all(&parcel_dir).unwrap();

    // 1. Seed TITLE.md
    let title_path = tree.parcel_title(slot_uid);
    let title_content = format!(
        "# TITLE — parcel{slot_uid}\n\n\
         - [2026-10-19T01:00:00Z] trigger commission=42 prompt=\"transcribe annals\"\n\
         - [2026-10-19T01:00:01Z] provision uid={slot_uid} archetype={identity} blueprint=village-alpine version=1\n\
         - [2026-10-19T01:00:05Z] session session_id={slot_uid}.0 incarnation=1\n"
    );
    fs::write(&title_path, title_content).unwrap();

    // 2. Seed DEED.md
    let deed_path = tree.parcel_deed(slot_uid);
    let deed_content = format!(
        "# DEED — parcel{slot_uid}\n\n\
         - identity: {identity}\n\
         - uid: {slot_uid}\n\
         - commission: 42\n\
         - blueprint: village-alpine v1\n"
    );
    fs::write(&deed_path, deed_content).unwrap();

    // 3. Seed PROPRIETOR.md
    let prop_path = tree.parcel_proprietor(slot_uid);
    let prop_content = format!(
        "# PROPRIETOR — {identity}\n\n\
         Active intelligence profile for parcel {slot_uid}.\n\
         Specialized in transcribing historical court records.\n"
    );
    fs::write(&prop_path, prop_content).unwrap();

    // 4. Seed home/ directory with files and subdirectories
    let home_dir = tree.parcel_home(slot_uid);
    fs::create_dir_all(home_dir.join("work")).unwrap();
    fs::create_dir_all(home_dir.join("config")).unwrap();
    fs::write(
        home_dir.join("README.md"),
        b"Agent private home workspace\n",
    )
    .unwrap();
    fs::write(
        home_dir.join("work/notes.txt"),
        b"Day 1: transcriptions completed.\n",
    )
    .unwrap();
    fs::write(
        home_dir.join("config/settings.json"),
        b"{\"editor\": \"vi\"}\n",
    )
    .unwrap();

    (tree, runtime, telemetry)
}

#[tokio::test]
async fn test_clean_hibernate_happy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let uid = 1001;
    let identity = name("scribe");

    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    let doc = stasis
        .hibernate(uid, "operator:cli")
        .await
        .expect("hibernate must succeed");

    // Assert step 3: Patron doc model returned
    assert_eq!(doc.frontmatter.patron, identity);
    assert_eq!(doc.frontmatter.source_uid, uid);
    assert_eq!(doc.frontmatter.commission, Some(42));
    assert_eq!(doc.frontmatter.blueprint, name("village-alpine"));
    assert_eq!(doc.frontmatter.blueprint_version, 1);
    assert!(doc
        .body
        .contains("Specialized in transcribing historical court records."));

    // Assert step 2: tavern/patrons/PROPRIETORscribe/ layout
    let patron_dir = tree.patron(&identity);
    assert!(patron_dir.is_dir(), "patron directory must exist");

    let patron_md = tree.patron_md(&identity);
    assert!(patron_md.is_file(), "patron PROPRIETOR.md must exist");
    let loaded_patron = PatronDoc::parse(&fs::read_to_string(&patron_md).unwrap()).unwrap();
    assert_eq!(loaded_patron.frontmatter.patron, identity);

    let patron_home_bak = tree.patron_home_backup(&identity);
    assert!(patron_home_bak.is_dir(), "home.uid.bak must exist");
    assert_eq!(
        fs::read(patron_home_bak.join("README.md")).unwrap(),
        b"Agent private home workspace\n"
    );
    assert_eq!(
        fs::read(patron_home_bak.join("work/notes.txt")).unwrap(),
        b"Day 1: transcriptions completed.\n"
    );
    assert_eq!(
        fs::read(patron_home_bak.join("config/settings.json")).unwrap(),
        b"{\"editor\": \"vi\"}\n"
    );

    // Assert manifest.json and checksums
    let manifest_path = tree.patron_manifest(&identity);
    assert!(manifest_path.is_file(), "manifest.json must exist");
    let manifest = BackupManifest::parse(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.patron, identity);
    assert_eq!(manifest.source_uid, uid);
    assert!(manifest.entries.contains_key("README.md"));
    assert!(manifest.entries.contains_key("work"));
    assert!(manifest.entries.contains_key("config"));

    // Verify each manifest entry
    for (entry_name, entry) in &manifest.entries {
        manifest
            .verify_entry(
                entry_name,
                entry.entry_type,
                entry.size_bytes,
                &entry.digest,
            )
            .expect("manifest entry verification must succeed");
    }

    // Assert step 4: Charter update
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.slots[&uid].status, SlotStatus::Hibernated);
    assert!(
        charter.parcels[&uid].stasis,
        "parcel lease stasis must be true"
    );
    assert_eq!(charter.sessions[&uid].status, SessionStatus::Stopped);

    // Assert step 4: Sled telemetry trace
    let key = AgentKey {
        commission: 42,
        uid,
    };
    let traces = telemetry.recent_traces(key, 10).unwrap();
    assert!(
        traces.iter().any(|t| t.event == "agent-hibernated"),
        "telemetry must record agent-hibernated trace"
    );

    // Assert compute freed
    let recorded = runtime.argv_log();
    assert!(
        recorded.iter().any(|argv| {
            argv.contains(&"rm".to_string())
                && (argv.contains(&"--force".to_string()) || argv.contains(&"-f".to_string()))
                && argv.contains(&identity.to_string())
        }),
        "runtime must execute podman rm --force {identity}"
    );

    // Assert step 5: Parcel directory released and metadata archived
    assert!(!tree.parcel(uid).exists(), "parcel dir must be released");
    let archive_dir = tree.parcel_archive(uid);
    assert!(archive_dir.is_dir(), "archive dir must exist");
    assert!(archive_dir.join("TITLE.md").is_file());
    assert!(archive_dir.join("DEED.md").is_file());
    assert!(archive_dir.join("PROPRIETOR.md").is_file());

    // Assert TITLE.md has TitleEntry::Hibernate and preserves append-only integrity
    let archived_title = fs::read_to_string(archive_dir.join("TITLE.md")).unwrap();
    let title_doc = TitleDoc::parse(&archived_title).unwrap();
    assert_eq!(title_doc.entries.len(), 4);
    match &title_doc.entries[3] {
        TitleEntry::Hibernate { patron, reason, .. } => {
            assert_eq!(patron, &identity);
            assert_eq!(reason, "operator:cli");
        }
        other => panic!("expected TitleEntry::Hibernate, got {other:?}"),
    }

    // Assert census view reports stasis state
    let census_views = list_census(&tree, Some(&telemetry)).unwrap();
    let parcel_view = census_views
        .iter()
        .find(|v| v.uid == uid)
        .expect("parcel view in census");
    assert!(parcel_view.stasis);
    assert_eq!(parcel_view.state, "stasis");
    assert_eq!(parcel_view.last_event.as_deref(), Some("hibernate"));
}

#[tokio::test]
async fn test_hibernate_preserves_delivery_queue_and_mail() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let uid = 1001;
    let identity = name("scribe");

    // 1. Seed courier delivery queue: courier/queues/1001.jsonl
    let queues_dir = tree.courier_queues();
    fs::create_dir_all(&queues_dir).unwrap();
    let queue_file = tree.session_queue(uid);
    let queued_data =
        b"{\"seq\":1,\"message\":\"pending directive\"}\n{\"seq\":2,\"message\":\"second task\"}\n";
    fs::write(&queue_file, queued_data).unwrap();

    // 2. Seed recipient mail spool: courier/mail/scribe/msg1.json
    let mail_spool = tree.courier_mail_recipient(&identity);
    fs::create_dir_all(&mail_spool).unwrap();
    let msg_file = mail_spool.join("msg1.json");
    let mail_data = b"{\"subject\":\"urgent memo\",\"from\":\"mayor\"}";
    fs::write(&msg_file, mail_data).unwrap();

    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    stasis
        .hibernate(uid, "decree:hibernate_agent")
        .await
        .expect("hibernate must succeed");

    // Verify delivery queue is completely intact
    assert!(queue_file.is_file(), "delivery queue must be preserved");
    assert_eq!(fs::read(&queue_file).unwrap(), queued_data);

    // Verify recipient mail spool is completely intact
    assert!(msg_file.is_file(), "mail spool message must be preserved");
    assert_eq!(fs::read(&msg_file).unwrap(), mail_data);
}

#[tokio::test]
async fn test_manifest_integrity_and_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let uid = 1001;
    let identity = name("scribe");

    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    stasis
        .hibernate(uid, "test:manifest")
        .await
        .expect("hibernate must succeed");

    let manifest_path = tree.patron_manifest(&identity);
    let manifest = BackupManifest::parse(&fs::read_to_string(&manifest_path).unwrap()).unwrap();

    // 1. Valid entry verification
    let readme_entry = manifest.entries.get("README.md").unwrap();
    manifest
        .verify_entry(
            "README.md",
            readme_entry.entry_type,
            readme_entry.size_bytes,
            &readme_entry.digest,
        )
        .expect("exact entry verifies");

    // 2. Tampered content digest detection
    let tampered_digest = "deadbeefcafebabe";
    let tamper_res = manifest.verify_entry(
        "README.md",
        readme_entry.entry_type,
        readme_entry.size_bytes,
        tampered_digest,
    );
    assert!(tamper_res.is_err(), "digest mismatch must be rejected");

    // 3. Tampered size detection
    let tamper_size_res = manifest.verify_entry(
        "README.md",
        readme_entry.entry_type,
        readme_entry.size_bytes + 1,
        &readme_entry.digest,
    );
    assert!(tamper_size_res.is_err(), "size mismatch must be rejected");

    // 4. Tampered type detection
    let tamper_type_res = manifest.verify_entry(
        "README.md",
        ManifestEntryType::Directory,
        readme_entry.size_bytes,
        &readme_entry.digest,
    );
    assert!(tamper_type_res.is_err(), "type mismatch must be rejected");

    // 5. Unknown entry detection
    let unknown_res = manifest.verify_entry(
        "intruder.sh",
        ManifestEntryType::File,
        100,
        &readme_entry.digest,
    );
    assert!(unknown_res.is_err(), "unrecorded entry must be rejected");
}

#[tokio::test]
async fn test_rollback_preflight_on_manor_or_unknown_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);

    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    // 1. Manor (1000) is strictly exempt (SPEC §10)
    let manor_err = stasis.hibernate(1000, "operator:cli").await.unwrap_err();
    match manor_err {
        StasisError::Exempt(1000) => {}
        other => panic!("expected StasisError::Exempt(1000), got {other:?}"),
    }

    // 2. Unknown unallocated slot (1042)
    let unknown_err = stasis.hibernate(1042, "operator:cli").await.unwrap_err();
    match unknown_err {
        StasisError::UnknownSlot(1042) => {}
        other => panic!("expected StasisError::UnknownSlot(1042), got {other:?}"),
    }

    // 3. Out of range slot (999)
    let range_err = stasis.hibernate(999, "operator:cli").await.unwrap_err();
    match range_err {
        StasisError::UnknownSlot(999) => {}
        other => panic!("expected StasisError::UnknownSlot(999), got {other:?}"),
    }

    // Verify zero mutations occurred: no patrons created, no container removed
    assert_eq!(fs::read_dir(tree.tavern_patrons()).unwrap().count(), 0);
    assert!(runtime.argv_log().is_empty());

    // Verify charter untouched
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.slots[&1001].status, SlotStatus::Active);
}

#[tokio::test]
async fn test_post_teardown_failure_marks_crashed_hibernating() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let uid = 1001;
    let identity = name("scribe");

    // Configure stasis with tight 10-byte size limit so step 2 exceeds it
    let stasis = ProductionStasis::new(tree.clone(), runtime.clone())
        .with_telemetry(telemetry.clone())
        .with_max_home_bytes(10);

    let err = stasis.hibernate(uid, "test:overflow").await.unwrap_err();

    match err {
        StasisError::PostTeardown { cause, detail } => {
            assert!(detail.contains("hibernation failed for uid 1001 after container teardown"));
            match *cause {
                StasisError::BackupSizeExceeded {
                    total_bytes,
                    max_bytes,
                } => {
                    assert_eq!(max_bytes, 10);
                    assert!(total_bytes > 10);
                }
                other => panic!("expected BackupSizeExceeded cause, got {other:?}"),
            }
        }
        other => panic!("expected StasisError::PostTeardown, got {other:?}"),
    }

    // Container teardown DID happen
    let recorded = runtime.argv_log();
    assert!(
        recorded
            .iter()
            .any(|argv| argv.contains(&identity.to_string())),
        "container teardown must have been called"
    );

    // Verify post-teardown failure state matrix:
    // 1. Charter reflects Crashed slot status and session status
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.slots[&uid].status, SlotStatus::Crashed);
    assert_eq!(charter.sessions[&uid].status, SessionStatus::Crashed);

    // 2. TITLE.md has TitleEntry::Crash recorded
    let title_path = tree.parcel_title(uid);
    let title_content = fs::read_to_string(&title_path).unwrap();
    let title_doc = TitleDoc::parse(&title_content).unwrap();
    assert!(
        title_doc.entries.iter().any(|e| matches!(e, TitleEntry::Crash { reason, .. } if reason.contains("hibernation failed after teardown"))),
        "TITLE.md must record TitleEntry::Crash"
    );

    // 3. Sled telemetry records session-hibernation-failed
    let key = AgentKey {
        commission: 42,
        uid,
    };
    let traces = telemetry.recent_traces(key, 10).unwrap();
    assert!(
        traces
            .iter()
            .any(|t| t.event == "session-hibernation-failed"),
        "telemetry must record session-hibernation-failed trace"
    );
}

#[tokio::test]
async fn test_decree_executor_integration() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let uid = 1001;
    let identity = name("scribe");

    let stasis = Arc::new(
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone()),
    );

    let executor =
        DecreeExecutor::new(tree.clone(), Config::default()).with_stasis(stasis as Arc<dyn Stasis>);

    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    let decree = Decree::HibernateAgent {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        uid,
    }
    .stamp(1)
    .unwrap();

    write_decree(&tree, &decree);

    let report = executor.scan().await.unwrap();
    assert_eq!(report.executed(), 1);

    // Assert stasis state in charter
    let charter = Charter::load(&tree.charter()).unwrap();
    assert!(charter.parcels[&uid].stasis);
    assert_eq!(charter.slots[&uid].status, SlotStatus::Hibernated);

    // Assert patron record created
    assert!(tree.patron(&identity).is_dir());
    assert!(tree.patron_home_backup(&identity).is_dir());
}
