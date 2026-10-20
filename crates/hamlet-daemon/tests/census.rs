//! Integration tests for TITLE chain-of-title, tamper detection, and census records
//! (SPEC §10; ADR-0039, ADR-0040).

use std::fs;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::pins::ToolchainPins;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::stasis::{TitleDoc, TitleEntry, TitleEntryKind};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::script_pinned_digests;
use hamlet_daemon::census::{
    clear_title_cache, format_epoch_iso8601, list_census, read_parcel_title, render_agent_list,
    set_global_census_telemetry, set_global_census_wake, stamp_parcel_title, CensusWake,
    Provisioner, RecordingCensusWake, TitleWriter,
};
use hamlet_daemon::mayor::ManorProvisioner;
use hamlet_daemon::podman::FakeRuntime;
use hamlet_daemon::telemetry::{AgentKey, Telemetry};

const BRIEF: &str = "# scribe\n\nPrinciples: record everything faithfully.";
const TEST_MANIFEST_SHA256: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid name")
}

fn setup_village() -> (tempfile::TempDir, HamletTree, Arc<FakeRuntime>, Blueprint) {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    Charter::empty().save_atomic(&tree.charter()).unwrap();

    let runtime = Arc::new(FakeRuntime::new());
    let pins = ToolchainPins::default_hamlet().unwrap();
    script_pinned_digests(&runtime, 1000, &pins, TEST_MANIFEST_SHA256);
    script_pinned_digests(&runtime, 1001, &pins, TEST_MANIFEST_SHA256);

    let brief_path = tree.register_brief(&name("scribe"));
    fs::create_dir_all(brief_path.parent().unwrap()).unwrap();
    fs::write(&brief_path, BRIEF).unwrap();

    let blueprint = Blueprint::default_village_alpine().unwrap();
    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    fs::create_dir_all(bp_path.parent().unwrap()).unwrap();
    fs::write(&bp_path, blueprint.render_toml(None)).unwrap();

    (tmp, tree, runtime, blueprint)
}

/// Scripted lifecycle leaves the exact golden TITLE sequence (SPEC §10; ADR-0039).
#[tokio::test]
async fn scripted_lifecycle_produces_exact_golden_title_sequence() {
    clear_title_cache();
    let (_tmp, tree, runtime, blueprint) = setup_village();

    let provisioner = Provisioner::new(
        tree.clone(),
        Config::default(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );

    // 1. Provisioning seeds Trigger and Provision entries
    let _lease = provisioner
        .provision_parcel(&name("scribe"), 42, &blueprint)
        .await
        .expect("provisioning must succeed");

    let uid = 1001;

    // 2. Session start
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::SessionIncarnation {
            timestamp: format_epoch_iso8601(1_700_000_100),
            session_id: "1001.0".to_string(),
            incarnation: 1,
        },
    )
    .expect("stamp session incarnation");

    // 3. Steer directive
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Steer {
            timestamp: format_epoch_iso8601(1_700_000_200),
            directive: "focus on deliverable drafting".to_string(),
        },
    )
    .expect("stamp steer");

    // 4. Git bundle apply
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_300),
            kind: "bundle".to_string(),
            target: "patch-01.bundle".to_string(),
            status: "applied".to_string(),
        },
    )
    .expect("stamp bundle apply");

    // 5. Deliverable submission
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_400),
            kind: "submission".to_string(),
            target: "freight/manuscript.pdf".to_string(),
            status: "submitted".to_string(),
        },
    )
    .expect("stamp submission");

    // 6. DEED promotion
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_500),
            kind: "deed".to_string(),
            target: "village-alpine.v2".to_string(),
            status: "promoted".to_string(),
        },
    )
    .expect("stamp deed promotion");

    // 7. Session crash (restartable)
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Crash {
            timestamp: format_epoch_iso8601(1_700_000_600),
            session_id: "1001.0".to_string(),
            exit_code: Some(137),
            restarts: 0,
            reason: "oom killed".to_string(),
        },
    )
    .expect("stamp crash");

    // 8. Session restart (incarnation 2)
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::SessionIncarnation {
            timestamp: format_epoch_iso8601(1_700_000_700),
            session_id: "1001.1".to_string(),
            incarnation: 2,
        },
    )
    .expect("stamp restart incarnation");

    // 9. Session spin-down / stopped
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_800),
            kind: "session".to_string(),
            target: "1001.1".to_string(),
            status: "stopped".to_string(),
        },
    )
    .expect("stamp session stopped");

    // Read and verify full document
    let doc = read_parcel_title(&tree, uid).expect("read and verify TITLE.md");
    assert_eq!(doc.entries.len(), 10, "exactly 10 entries stamped");

    let expected_kinds = [
        TitleEntryKind::Trigger,
        TitleEntryKind::Provision,
        TitleEntryKind::SessionIncarnation,
        TitleEntryKind::Steer,
        TitleEntryKind::Promotion,
        TitleEntryKind::Promotion,
        TitleEntryKind::Promotion,
        TitleEntryKind::Crash,
        TitleEntryKind::SessionIncarnation,
        TitleEntryKind::Promotion,
    ];

    for (i, (entry, &expected_kind)) in doc.entries.iter().zip(&expected_kinds).enumerate() {
        assert_eq!(
            entry.kind(),
            expected_kind,
            "entry {i} kind mismatch: {:?}",
            entry
        );
    }

    // Verify append-only property against an initial prefix
    let prefix = &doc.entries[0..4];
    assert!(
        doc.verify_append_only(prefix).is_ok(),
        "document must be a strict append-only extension of its prefix"
    );

    // Verify round-trip formatting
    let rendered = doc.render();
    let re_parsed = TitleDoc::parse(&rendered).expect("must parse rendered output");
    assert_eq!(doc.entries, re_parsed.entries);
}

/// Tamper detection fires sled trace and Mayor wake on non-append mutations (SPEC §10).
#[tokio::test]
async fn tamper_detection_fires_sled_trace_and_mayor_wake() {
    clear_title_cache();
    let (_tmp, tree, runtime, blueprint) = setup_village();

    // 1. Setup wake recorder and sled telemetry
    let wake = Arc::new(RecordingCensusWake::new());
    set_global_census_wake(Arc::clone(&wake) as Arc<dyn CensusWake>);

    let config = Config::default();
    let telemetry =
        Arc::new(Telemetry::open_for_village(tree.root(), &config).expect("open sled telemetry"));
    set_global_census_telemetry(Arc::clone(&telemetry));

    let provisioner = Provisioner::new(
        tree.clone(),
        config.clone(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );

    let _lease = provisioner
        .provision_parcel(&name("scribe"), 42, &blueprint)
        .await
        .unwrap();

    let uid = 1001;

    // Initial read populates cache and passes
    let writer = TitleWriter::new(tree.clone())
        .with_wake(Arc::clone(&wake) as Arc<dyn CensusWake>)
        .with_telemetry(Arc::clone(&telemetry));

    let (_doc, tampered) = writer.read_and_verify(uid).expect("initial read clean");
    assert!(!tampered, "initially not tampered");
    assert_eq!(wake.events().len(), 0);

    // 2. Simulate adversary mutating historical entry in place
    let title_path = tree.parcel_title(uid);
    let content = fs::read_to_string(&title_path).unwrap();
    // Tamper with the blueprint version in the historical provision line
    let tampered_content = content.replace("version=1", "version=99");
    fs::write(&title_path, tampered_content).unwrap();

    // 3. Next read detects non-append mutation
    let (_doc_after, tampered_after) = writer
        .read_and_verify(uid)
        .expect("read returns doc with tampered flag");
    assert!(tampered_after, "tampering must be detected");

    // Mayor wake must have fired
    assert_eq!(
        wake.events().len(),
        1,
        "Mayor wake must fire on tamper detection"
    );
    let (tampered_uid, ref tampered_identity, ref detail) = wake.events()[0];
    assert_eq!(tampered_uid, uid);
    assert_eq!(*tampered_identity, name("scribe"));
    assert!(detail.contains("non-append mutation"), "detail: {detail}");

    // Sled telemetry must carry `census-title-tamper` trace
    let traces = telemetry
        .recent_traces(AgentKey { commission: 0, uid }, 10)
        .expect("query sled traces");
    let has_tamper_trace = traces
        .iter()
        .any(|t| t.event == "census-title-tamper" && t.detail.contains("non-append mutation"));
    assert!(
        has_tamper_trace,
        "sled must record census-title-tamper trace: {traces:?}"
    );
}

/// Manor is exempt from hibernation and list_census matches charter state (SPEC §10).
#[tokio::test]
async fn manor_exempt_title_and_census_listing_matches_charter() {
    clear_title_cache();
    let (_tmp, tree, runtime, blueprint) = setup_village();

    // 1. Provision Manor
    let config = Config::default();
    let manor_prov = ManorProvisioner::new(
        tree.clone(),
        config.clone(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    manor_prov
        .provision(&blueprint)
        .await
        .expect("manor provision succeeds");

    // Verify Manor TITLE.md exists at keep/census/manor/TITLE.md
    let manor_title = tree.manor_title();
    assert!(
        manor_title.is_file(),
        "manor TITLE.md must exist at keep/census/manor/TITLE.md"
    );
    let manor_title_content = fs::read_to_string(&manor_title).unwrap();
    assert!(manor_title_content.starts_with("# TITLE — manor"));
    assert!(manor_title_content.contains("provision uid=1000 archetype=manor"));

    // 2. Provision agent parcel 1001 with commission
    let mut charter = Charter::load(&tree.charter()).unwrap();
    let comm_slug = name("great-work");
    let comm_id = charter.create_commission(comm_slug.clone()).unwrap();
    charter.save_atomic(&tree.charter()).unwrap();

    let provisioner = Provisioner::new(
        tree.clone(),
        config,
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    provisioner
        .provision_parcel(&name("scribe"), comm_id, &blueprint)
        .await
        .unwrap();

    // 3. Query census view
    let views = list_census(&tree, None).expect("list_census succeeds");
    assert_eq!(views.len(), 2, "manor + 1 parcel");

    // Manor entry
    let manor_view = &views[0];
    assert_eq!(manor_view.uid, 1000);
    assert_eq!(manor_view.identity, name("manor"));
    assert_eq!(manor_view.state, "exempt");
    assert_eq!(manor_view.commission, None);
    assert!(!manor_view.stasis);

    // Agent entry
    let agent_view = &views[1];
    assert_eq!(agent_view.uid, 1001);
    assert_eq!(agent_view.identity, name("scribe"));
    assert_eq!(agent_view.commission, Some(comm_id));
    assert_eq!(agent_view.commission_slug, Some(comm_slug));
    assert_eq!(agent_view.state, "idle");
    assert!(!agent_view.stasis);

    // 4. Render ASCII table output
    let table = render_agent_list(&views);
    assert!(table.contains("UID"));
    assert!(table.contains("IDENTITY"));
    assert!(table.contains("COMMISSION"));
    assert!(table.contains("STATE"));
    assert!(table.contains("1000"));
    assert!(table.contains("manor"));
    assert!(table.contains("exempt"));
    assert!(table.contains("1001"));
    assert!(table.contains("scribe"));
    assert!(table.contains("great-work"));
}
