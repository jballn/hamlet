//! Adversarial comms suite (Sprint 04 step 05 QA; SPEC §8, ADR-0019…0023).
//!
//! Written by the QA role to ATTACK the committed planes — not to mirror
//! their happy-path tests. CONVENTIONS §9 doctrine: tempdir trees, scripted
//! seams (FakeRuntime / HttpClient), no podman required (the guarded freight
//! tamper test lives in `freight_guarded.rs` and runs under
//! `HAMLET_PODMAN_TESTS=1`).
//!
//! Planes attacked: Mail (ADR-0019) — header-injection matrix, oversized
//! fields, spoofed `From:` vs daemon-asserted identity, bounce paths,
//! partial-file visibility, zero duplicate delivery across kill -9 restarts,
//! per-recipient FIFO under a mixed-writer storm. Watcher (ADR-0019 cl. 5–6)
//! — churn, downtime events, ordering determinism. Freight (ADR-0021) —
//! world-writable variants, foreign UIDs, named groups, symlink escapes,
//! promotion-read under strict ACLs. Dispatch (ADR-0020/0023) — allowlist
//! matrix, secret hygiene, deterministic retry schedule, dead-letter budget,
//! malformed-JSON matrix with plane liveness. Inbound webhooks (ADR-0022) —
//! bad auth, oversize bodies, path-traversal topics, content-type smuggling.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::{Config, WebhookRoute};
use hamlet_core::dispatch::{
    file_name, InboxResponse, Method, OutboxRequest, Outcome, RetryPolicy, UrlAllowlist,
    DISPATCH_VERSION,
};
use hamlet_core::freight::ViolationKind;
use hamlet_core::mail::{is_incoming, spool_file_name, Mail, MailError, MailHeader, MailSender};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{CorrelationId, SessionEnvelope, SessionStatus};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::dispatch::{
    spawn_dispatch_plane, DispatchOutcome, DispatchPlane, HttpClient, WireRequest, WireResponse,
};
use hamlet_daemon::mailroute::{spawn_mail_plane, MailActor, MailRouter, RouteOutcome};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{AgentKey, Telemetry};
use hamlet_daemon::web::{spawn_web_server, RecordingWebhookWake, WebhookService, AUTH_HEADER};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;
const MAYOR: u32 = 1000;
const SCRIBE: u32 = 1001;
const HERALD: u32 = 1002;

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

// ===========================================================================
// Shared fixtures
// ===========================================================================

/// Village fixture for mail/router-level attacks. No parcels needed: the
/// router's cross-check is charter-only (SPEC §8.1; ADR-0019 cl. 2–4).
struct MailFixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    supervisor: Arc<Supervisor>,
    router: Arc<MailRouter>,
}

fn mail_fixture(tweak: impl FnOnce(&mut Config)) -> MailFixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    charter.save_atomic(&tree.charter()).unwrap();

    let mut config = Config::default();
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
    MailFixture {
        _tmp: tmp,
        tree,
        supervisor,
        router,
    }
}

/// Render a canonical mail for spool writes (external-writer simulation).
fn mail_text(from: &str, to: u32, subject: &str, body: &str) -> String {
    Mail {
        header: MailHeader {
            from: MailSender::parse(from).unwrap(),
            to,
            subject: subject.to_owned(),
            date: "unix-1700000000".to_owned(),
        },
        body: format!("{body}\n"),
    }
    .render()
}

/// Direct spool placement with the conforming name grammar.
fn write_spool(tree: &HamletTree, recipient: u32, seq: u64, writer: u32, text: &str) -> SafeName {
    let dir = tree.courier_mail_recipient(&name(&recipient.to_string()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = spool_file_name(seq, writer, &format!("{seq:x}")).unwrap();
    std::fs::write(dir.join(&file), text).unwrap();
    name(&file)
}

fn spool_names(tree: &HamletTree, recipient: u32) -> Vec<String> {
    let dir = tree.courier_mail_recipient(&name(&recipient.to_string()));
    let out: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| is_incoming(n))
                .collect()
        })
        .unwrap_or_default();
    out
}

fn pending_texts(supervisor: &Supervisor, uid: u32) -> Vec<String> {
    supervisor
        .queues()
        .pending(uid)
        .unwrap()
        .into_iter()
        .map(|envelope| match envelope {
            SessionEnvelope::Mail { text, .. } => text,
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

async fn wait_until(cond: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ===========================================================================
// A. Mail format: injection matrix (core parse — the first wall)
// ===========================================================================

/// Every header-injection shape must die at `Mail::parse` with a typed
/// error — CR anywhere, embedded newlines splitting the fixed preamble,
/// spoofed/traversal From values (ADR-0019 cl. 1).
#[test]
fn injection_matrix_cr_newlines_and_spoofed_from() {
    let base = "From: 1042\nTo: 1001\nSubject: S\nDate: D\n\nBody.\n";

    for pos in ["S", "D", "Body."] {
        let cr = base.replace(pos, &format!("{pos}\r"));
        assert_eq!(Mail::parse(&cr), Err(MailError::CarriageReturn));
    }

    // Embedded newline inside Subject: the preamble splits and slot 4 no
    // longer starts with `Date:`.
    let injected_subject = "From: 1042\nTo: 1001\nSubject: S\nX-Injected: evil\nDate: D\n\nBody.\n";
    assert!(matches!(
        Mail::parse(injected_subject),
        Err(MailError::HeaderMissing { index: 3 })
    ));

    // Newline inside From (spoof with a second header line).
    let injected_from = "From: 1042\nX-Real: 9999\nTo: 1001\nSubject: S\nDate: D\n\nBody.\n";
    assert!(matches!(
        Mail::parse(injected_from),
        Err(MailError::HeaderMissing { index: 1 })
    ));

    // A CR-laden body still dies on the whole-message check.
    let cr_body = "From: 1042\nTo: 1001\nSubject: S\nDate: D\n\nBody\r\nFrom: 1000\n";
    assert_eq!(Mail::parse(cr_body), Err(MailError::CarriageReturn));

    // Spoofed / traversal From values: out-of-range UIDs and path shapes.
    for bad in ["999", "3000", "../etc/passwd", "a/b"] {
        assert!(matches!(
            Mail::parse(&base.replace("From: 1042", &format!("From: {bad}"))),
            Err(MailError::InvalidFrom { .. })
        ));
    }
    // A valid spoof shape (any in-range slot) PARSES — identity claims are
    // constrained to the slot table only; section B covers routing impact.
    assert!(Mail::parse(&base.replace("From: 1042", "From: 1000")).is_ok());
}

/// Oversized fields die at their exact bounds (ADR-0019 cl. 1): subject
/// ≤128, date ≤64, body ≤64 KiB, whole message bounded.
#[test]
fn oversized_fields_rejected_at_exact_bounds() {
    let base = "From: 1042\nTo: 1001\nSubject: S\nDate: D\n\nBody.\n";

    let s128 = format!("Subject: {}", "x".repeat(128));
    assert!(Mail::parse(&base.replace("Subject: S", &s128)).is_ok());
    let s129 = format!("Subject: {}", "x".repeat(129));
    assert!(matches!(
        Mail::parse(&base.replace("Subject: S", &s129)),
        Err(MailError::InvalidSubject { .. })
    ));

    let d64 = format!("Date: {}", "x".repeat(64));
    assert!(Mail::parse(&base.replace("Date: D", &d64)).is_ok());
    let d65 = format!("Date: {}", "x".repeat(65));
    assert!(matches!(
        Mail::parse(&base.replace("Date: D", &d65)),
        Err(MailError::InvalidDate { .. })
    ));

    let body = "y".repeat(hamlet_core::mail::MAX_BODY_BYTES);
    let big = format!("From: 1042\nTo: 1001\nSubject: S\nDate: D\n\n{body}\n");
    assert!(Mail::parse(&big).is_ok());
    let bigger = format!(
        "From: 1042\nTo: 1001\nSubject: S\nDate: D\n\n{}\n",
        "y".repeat(hamlet_core::mail::MAX_BODY_BYTES + 1)
    );
    assert!(matches!(
        Mail::parse(&bigger),
        Err(MailError::BodyTooLarge { .. })
    ));

    // Whole-message cap (headers alone can exceed it).
    let huge = format!(
        "From: 1042\nTo: 1001\nSubject: {}\nDate: D\n\nBody.\n",
        "x".repeat(70_000)
    );
    assert!(matches!(
        Mail::parse(&huge),
        Err(MailError::MessageTooLarge { .. })
    ));
}

// ===========================================================================
// B. Spoofed From vs daemon-asserted identity (router level)
// ===========================================================================

/// An agent spool file may claim ANY in-range `From:` — the router delivers
/// under the claimed identity (committed ADR-0019 behavior: headers are
/// trusted for well-formed mail). The test pins it so a later hardening
/// change shows up as a deliberate diff, not a regression.
#[test]
fn spoofed_from_delivers_under_claimed_identity() {
    let fx = mail_fixture(|_| {});
    // HERALD (1002) writes to SCRIBE's spool claiming to be the Mayor.
    let text = mail_text("1000", SCRIBE, "Decree", "Herald says so.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);

    assert!(matches!(
        fx.router.route(&name("1001"), &file).unwrap(),
        RouteOutcome::Delivered { .. }
    ));
    let queued = pending_texts(&fx.supervisor, SCRIBE);
    assert_eq!(queued.len(), 1);
    // The recipient sees the SPOOFED identity — no writer cross-check.
    let parsed = Mail::parse(&queued[0]).unwrap();
    assert_eq!(parsed.header.from, MailSender::Uid(MAYOR));
}

/// Bounce targeting on well-formed mail follows the (spoofable) header
/// `From:`, NOT the file-name writer — while malformed mail bounces to the
/// file-name writer. QA pins the asymmetry: an agent that forges
/// `From: 1001` in a conforming-named file sent by writer 1002 gets its
/// bounce at 1001's door. The `SpoolRef.writer` doc says "the daemon
/// cross-checks file owner"; no such cross-check exists for well-formed
/// mail (finding QA04-01, HANDOFF).
#[test]
fn spoofed_from_bounce_targets_header_not_writer() {
    let fx = mail_fixture(|_| {});
    // HERALD (writer 1002) sends to slotless 1005 claiming From: SCRIBE.
    std::fs::create_dir_all(fx.tree.courier_mail_recipient(&name("1005"))).unwrap();
    let text = mail_text("1001", 1005, "Anyone home?", "Body.");
    let file = write_spool(&fx.tree, 1005, 1, HERALD, &text);

    match fx.router.route(&name("1005"), &file).unwrap() {
        RouteOutcome::Bounced { to, .. } => {
            assert_eq!(to, Some(SCRIBE), "bounce went to the spoofed uid");
        }
        other => panic!("expected bounce, got {other:?}"),
    }
    let bounced_to: Vec<u32> = [SCRIBE, HERALD]
        .iter()
        .copied()
        .filter(|uid| !spool_names(&fx.tree, *uid).is_empty())
        .collect();
    assert_eq!(
        bounced_to,
        vec![SCRIBE],
        "only the spoofed uid got the bounce"
    );

    // Contrast: the SAME content malformed → bounce targets the file-name
    // writer (1002), because headers are unusable.
    let dir = fx.tree.courier_mail_recipient(&name("1005"));
    std::fs::write(dir.join("000000000002-1002-ab.mail"), b"garbage").unwrap();
    match fx
        .router
        .route(&name("1005"), &name("000000000002-1002-ab.mail"))
        .unwrap()
    {
        RouteOutcome::Bounced { to, .. } => assert_eq!(to, Some(HERALD)),
        other => panic!("expected malformed bounce, got {other:?}"),
    }
}

/// The daemon send API asserts identity from the actor — no caller input
/// can claim a `From:` the daemon did not choose (SPEC §8.1; ADR-0019).
#[test]
fn daemon_send_asserts_actor_identity() {
    let fx = mail_fixture(|_| {});

    let path = fx
        .router
        .send(MailActor::Mayor, SCRIBE, "From the Mayor", "Body.", None)
        .unwrap();
    let parsed = Mail::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(parsed.header.from, MailSender::Uid(MAYOR));

    let path = fx
        .router
        .send(MailActor::Agent(HERALD), SCRIBE, "S", "B.", None)
        .unwrap();
    let parsed = Mail::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(parsed.header.from, MailSender::Uid(HERALD));

    // Even a patron-named actor cannot claim a slot uid.
    let path = fx
        .router
        .send(MailActor::Patron(name("anna")), SCRIBE, "S", "B.", None)
        .unwrap();
    let parsed = Mail::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(parsed.header.from, MailSender::Name(name("anna")));

    // The send API re-parses through the same wall incoming mail passes.
    let bad_subjects: Vec<String> = vec!["".to_owned(), "x".repeat(129), " lead".to_owned()];
    for bad_subject in &bad_subjects {
        assert!(fx
            .router
            .send(MailActor::Mayor, SCRIBE, bad_subject, "B.", None)
            .is_err());
    }
}

// ===========================================================================
// C. Bounce paths (unallocated / hibernating / crashed / overflow)
// ===========================================================================

/// The four undeliverable classes each bounce with a typed reason and are
/// consumed from the spool (ADR-0019 cl. 4). Hibernation is the new class
/// versus step-02's tests (which covered crashed only).
#[test]
fn bounce_matrix_unallocated_hibernating_crashed_overflow() {
    let fx = mail_fixture(|c| c.sessions.queue_depth = 1);

    // 1. Unallocated: no charter slot at all (uid 1005 never reserved).
    std::fs::create_dir_all(fx.tree.courier_mail_recipient(&name("1005"))).unwrap();
    let text = mail_text("1002", 1005, "Unallocated", "Body.");
    let file = write_spool(&fx.tree, 1005, 1, HERALD, &text);
    match fx.router.route(&name("1005"), &file).unwrap() {
        RouteOutcome::Bounced { reason, to } => {
            assert_eq!(reason.to_string(), "no slot allocated for UID 1005");
            assert_eq!(to, Some(HERALD));
        }
        other => panic!("expected NoSlot bounce, got {other:?}"),
    }

    // 2. Hibernating: charter session record in hibernation refuses.
    let mut charter = Charter::load(&fx.tree.charter()).unwrap();
    charter.sessions.insert(
        SCRIBE,
        hamlet_core::session::SessionRecord {
            id: hamlet_core::session::SessionId::new(SCRIBE, 0).unwrap(),
            status: SessionStatus::Hibernating,
            restart_count: 0,
        },
    );
    charter.save_atomic(&fx.tree.charter()).unwrap();
    let text = mail_text("1002", SCRIBE, "Hib?", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);
    match fx.router.route(&name("1001"), &file).unwrap() {
        RouteOutcome::Bounced { reason, .. } => {
            assert!(
                reason
                    .to_string()
                    .contains("unavailable (state: hibernating)"),
                "{reason}"
            );
        }
        other => panic!("expected SessionUnavailable bounce, got {other:?}"),
    }

    // 3. Crashed: same class, different state string.
    let mut charter = Charter::load(&fx.tree.charter()).unwrap();
    charter.sessions.insert(
        SCRIBE,
        hamlet_core::session::SessionRecord {
            id: hamlet_core::session::SessionId::new(SCRIBE, 0).unwrap(),
            status: SessionStatus::Crashed,
            restart_count: 3,
        },
    );
    charter.save_atomic(&fx.tree.charter()).unwrap();
    let text = mail_text("1002", SCRIBE, "Crashed?", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 2, HERALD, &text);
    match fx.router.route(&name("1001"), &file).unwrap() {
        RouteOutcome::Bounced { reason, .. } => {
            assert!(
                reason.to_string().contains("unavailable (state: crashed)"),
                "{reason}"
            );
        }
        other => panic!("expected crashed bounce, got {other:?}"),
    }

    // 4. Overflow: queue at depth bound refuses the next envelope.
    let text = mail_text("1001", HERALD, "Fill 1", "Body.");
    let file = write_spool(&fx.tree, HERALD, 1, SCRIBE, &text);
    assert!(matches!(
        fx.router.route(&name("1002"), &file).unwrap(),
        RouteOutcome::Delivered { .. }
    ));
    let text = mail_text("1001", HERALD, "Fill 2", "Body.");
    let file = write_spool(&fx.tree, HERALD, 2, SCRIBE, &text);
    match fx.router.route(&name("1002"), &file).unwrap() {
        RouteOutcome::Bounced { reason, to } => {
            assert_eq!(
                reason.to_string(),
                "delivery queue for UID 1002 full (depth 1)"
            );
            assert_eq!(to, Some(SCRIBE));
        }
        other => panic!("expected overflow bounce, got {other:?}"),
    }

    // Every original was consumed; the spools hold only bounces now.
    // The overflow bounce targeted SCRIBE (the From header), so exactly one
    // envelope remains: that bounce.
    assert!(spool_names(&fx.tree, 1005).is_empty());
    assert_eq!(spool_names(&fx.tree, SCRIBE).len(), 1);
    assert_eq!(pending_texts(&fx.supervisor, HERALD).len(), 1);
}

/// A named sender (patron) with an undeliverable recipient dies with a
/// trace — NO bounce is written anywhere, so chains cannot loop
/// (ADR-0019 cl. 4: depth bounded at 2 by construction).
#[test]
fn named_sender_bounce_dies_without_chain() {
    let fx = mail_fixture(|_| {});
    std::fs::create_dir_all(fx.tree.courier_mail_recipient(&name("1005"))).unwrap();
    let text = mail_text("patron-anna", 1005, "Hello?", "Body.");
    let file = write_spool(&fx.tree, 1005, 1, SCRIBE, &text);

    match fx.router.route(&name("1005"), &file).unwrap() {
        RouteOutcome::Bounced { to, .. } => assert_eq!(to, None),
        other => panic!("expected bounce, got {other:?}"),
    }
    // Nothing written into ANY spool: the chain dies here.
    for uid in [SCRIBE, HERALD] {
        assert!(spool_names(&fx.tree, uid).is_empty(), "uid {uid} got mail");
    }
    // The original is consumed (never re-scanned on restart).
    let read = fx.tree.courier_mail_read(&name("1005"));
    let mut acked: Vec<String> = std::fs::read_dir(&read)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    acked.sort();
    assert_eq!(acked, vec![file.as_str()]);
}

// ===========================================================================
// D. Partial-file visibility (writer crash mid-rename)
// ===========================================================================

/// A writer that crashes between temp-write and rename leaves a hidden
/// `.tmp-*` file behind. The router must never treat it as incoming — not
/// on the event path, not on restart recovery (ADR-0019 cl. 2).
#[test]
fn writer_crash_mid_rename_temp_never_routed() {
    let fx = mail_fixture(|_| {});
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    std::fs::create_dir_all(&dir).unwrap();

    // The "crashed writer" left a hidden temp with perfectly valid mail.
    let valid = mail_text("1002", SCRIBE, "Lost in transit?", "Body.");
    std::fs::write(dir.join(".tmp-12345-crash.mail"), &valid).unwrap();

    // Event path: the scan sees zero incoming files.
    let report = fx.router.route_recipient(&name("1001")).unwrap();
    assert_eq!(report.scanned, 0, "hidden temp must not be scanned");
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 0);

    // Restart recovery: same guarantee after a full daemon death.
    let report = fx.router.recover_spools().unwrap();
    assert_eq!(report.scanned, 0);
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 0);

    // The temp survives untouched (the daemon never deletes foreign files).
    assert!(dir.join(".tmp-12345-crash.mail").exists());

    // A real (visible) file still routes — the temp changed nothing.
    let text = mail_text("1002", SCRIBE, "Live", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);
    assert!(matches!(
        fx.router.route(&name("1001"), &file).unwrap(),
        RouteOutcome::Delivered { .. }
    ));
}

// ===========================================================================
// E. Zero duplicate delivery across kill -9 restarts
// ===========================================================================

/// The CONVENTIONS §3 r6 contract, adversarially: three mails pending, the
/// daemon "dies" (all objects dropped — the kill -9 shape) after routing
/// exactly one. Two restarts follow. The queue must hold each envelope
/// EXACTLY ONCE: A from its pre-death persistence, B and C from recovery,
/// never re-delivered from `.read/` or the spool (ADR-0019 cl. 3).
#[test]
fn kill_restart_zero_duplicates_across_two_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    charter.save_atomic(&tree.charter()).unwrap();

    let data_dir = tmp.path().join(".hamlet-data");
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));

    // Three mails in the spool (writer HERALD, seqs 1..3).
    for seq in [1u64, 2, 3] {
        let text = mail_text("1002", SCRIBE, &format!("msg-{seq}"), "Body.");
        write_spool(&tree, SCRIBE, seq, HERALD, &text);
    }

    // Life 1: the daemon routes mail #1 and dies mid-scan (kill -9).
    {
        let config = Config::default();
        let telemetry = Arc::new(Telemetry::open(&data_dir).unwrap());
        let supervisor = Arc::new(Supervisor::new(
            tree.clone(),
            config,
            Arc::clone(&spawner),
            Arc::clone(&telemetry),
        ));
        let router = MailRouter::new(Arc::clone(&supervisor));
        let file_a = name(&spool_file_name(1, HERALD, "1").unwrap());
        assert!(matches!(
            router.route(&name("1001"), &file_a).unwrap(),
            RouteOutcome::Delivered { .. }
        ));
        // Kill: drop everything (no graceful stop, no flush).
    }

    // Life 2: fresh supervisor + telemetry over the same tree/sled dir;
    // recovery scan as the startup hook (the production restart path).
    // Block scope = kill -9: every handle drops before life 3.
    {
        let config = Config::default();
        let telemetry = Arc::new(Telemetry::open(&data_dir).unwrap());
        let supervisor = Arc::new(Supervisor::new(
            tree.clone(),
            config,
            Arc::clone(&spawner),
            Arc::clone(&telemetry),
        ));
        let router = MailRouter::new(Arc::clone(&supervisor));
        let report = router.recover_spools().unwrap();
        assert_eq!(
            (report.scanned, report.delivered, report.bounced),
            (2, 2, 0),
            "recovery sees only the two un-acked mails"
        );

        // Exactly three envelopes, each once, in FIFO order.
        let canonical = |seq: u64| {
            Mail::parse(&mail_text("1002", SCRIBE, &format!("msg-{seq}"), "Body."))
                .unwrap()
                .render()
        };
        assert_eq!(
            pending_texts(&supervisor, SCRIBE),
            vec![canonical(1), canonical(2), canonical(3)],
            "zero duplicates, zero drops"
        );
    }

    // Life 3: a second restart over the SAME tree. Recovery must be a no-op
    // scan — `.read/` is never rescanned, the spool is empty.
    let config = Config::default();
    let telemetry = Arc::new(Telemetry::open(&data_dir).unwrap());
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        Arc::clone(&spawner),
        Arc::clone(&telemetry),
    ));
    let router = MailRouter::new(Arc::clone(&supervisor));
    let report = router.recover_spools().unwrap();
    assert_eq!(report.scanned, 0, "second restart scans nothing");
    assert_eq!(
        pending_texts(&supervisor, SCRIBE).len(),
        3,
        "no re-delivery on the second restart"
    );
}

// ===========================================================================
// F. Per-recipient FIFO under a mixed-writer storm
// ===========================================================================

/// 3 writers × 20 mails to one recipient, written interleaved and out of
/// seq order on disk. Per-recipient delivery must equal the lexicographic
/// spool scan order — deterministic regardless of event arrival (ADR-0019
/// cl. 2: reactions are hints; the sorted scan is the FIFO).
#[test]
fn fifo_storm_mixed_writers_deterministic_order() {
    const WRITERS: [u32; 3] = [MAYOR, HERALD, 1003];
    const PER_WRITER: u64 = 20;
    let fx = mail_fixture(|c| c.sessions.queue_depth = 100);

    // Allocate the extra writer slot so its uid is a charter slot.
    {
        let mut charter = Charter::load(&fx.tree.charter()).unwrap();
        assert_eq!(
            charter
                .allocate_slot(name("chronicler"), Some(COMMISSION))
                .unwrap(),
            1003
        );
        charter.save_atomic(&fx.tree.charter()).unwrap();
    }

    // Write all mail to SCRIBE, interleaved writers, out of order per writer.
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    std::fs::create_dir_all(&dir).unwrap();
    for (w, &writer) in WRITERS.iter().enumerate() {
        for seq in (1..=PER_WRITER).rev() {
            let text = mail_text(
                &writer.to_string(),
                SCRIBE,
                &format!("w{w}-s{seq}"),
                "Body.",
            );
            let file = spool_file_name(seq, writer, &format!("{seq:x}{w}")).unwrap();
            std::fs::write(dir.join(&file), text).unwrap();
        }
    }

    // One sequential scan of the recipient spool.
    let report = fx.router.route_recipient(&name("1001")).unwrap();
    assert_eq!(report.scanned, 3 * PER_WRITER as usize);
    assert_eq!(report.delivered, 3 * PER_WRITER as usize);
    assert_eq!(report.bounced, 0);

    // FIFO = lexicographic scan order of the spool names.
    let mut expected_names: Vec<String> = Vec::new();
    for (w, writer) in WRITERS.iter().enumerate() {
        for seq in 1..=PER_WRITER {
            expected_names.push(spool_file_name(seq, *writer, &format!("{seq:x}{w}")).unwrap());
        }
    }
    expected_names.sort();

    let queued_subjects: Vec<String> = pending_texts(&fx.supervisor, SCRIBE)
        .iter()
        .map(|t| t.lines().nth(2).unwrap().to_owned())
        .collect();
    let expected_subjects: Vec<String> = expected_names
        .iter()
        .map(|n| {
            let parts: Vec<&str> = n.split('-').collect();
            let seq: u64 = parts[0].parse().unwrap();
            let writer: u32 = parts[1].parse().unwrap();
            let w_index = WRITERS.iter().position(|w| *w == writer).unwrap();
            format!("Subject: w{w_index}-s{seq}")
        })
        .collect();
    assert_eq!(queued_subjects, expected_subjects, "FIFO violated");

    // Every file acked; queue depth exact.
    assert!(spool_names(&fx.tree, SCRIBE).is_empty());
    assert_eq!(
        fx.supervisor.queues().len(SCRIBE).unwrap(),
        3 * PER_WRITER as usize
    );
}

// QA-APPEND-MARKER (sections G–J appended below)

// ===========================================================================
// G. Watcher: churn, downtime events, ordering determinism
// ===========================================================================

/// Rapid create/delete churn inside the quiesce window cancels (ADR-0019
/// cl. 6): 40 blips produce ZERO reactions; a survivor that outlives the
/// window produces exactly one.
#[tokio::test]
async fn churn_blips_never_react_survivor_does() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let dispatcher = hamlet_daemon::watch::Dispatcher::new();
    let mut rx = dispatcher.register("collector");
    let dir = tree.courier_mail_recipient(&name("1001"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut handle =
        hamlet_daemon::watch::spawn_tree_watcher(tree.clone(), dispatcher, || {}).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await; // watch attach

    // 40 create/delete blips of CONFORMING names (is_incoming), each pair
    // well inside the 250 ms quiesce window.
    for i in 0..40u64 {
        let f = dir.join(format!("{i:012}-1002-ff.mail"));
        std::fs::write(&f, b"blip").unwrap();
        std::fs::remove_file(&f).unwrap();
    }
    // No reaction may have dispatched for the churn.
    assert!(
        tokio::time::timeout(Duration::from_millis(700), rx.recv())
            .await
            .is_err(),
        "churn blips must cancel inside the quiesce window"
    );

    // A survivor (outlives the window) dispatches exactly one reaction.
    std::fs::write(dir.join("000000009999-1002-ab.mail"), b"real").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .is_ok(),
        "survivor must dispatch"
    );
    handle.stop().await;
}

/// Events during daemon downtime: files written while no watcher/plane is
/// running are TREE-VISIBLE, so the restart's recovery scan must process
/// them — no lost reactions (CONVENTIONS §3 r6; ADR-0019 cl. 3, ADR-0023).
#[tokio::test]
async fn downtime_files_recovered_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    charter.save_atomic(&tree.charter()).unwrap();

    // "Downtime": the mail AND dispatch files land before any plane exists.
    let mail = mail_text("1002", SCRIBE, "downtime-mail", "Body.");
    write_spool(&tree, SCRIBE, 1, HERALD, &mail);
    let outbox_id = CorrelationId::parse("d-1-1001.0").unwrap();
    std::fs::create_dir_all(tree.courier_dispatch_outbox()).unwrap();
    std::fs::write(
        tree.courier_dispatch_outbox().join(file_name(&outbox_id)),
        r#"{"correlation":"d-1-1001.0","dispatch_version":1,"method":"post","url":"https://hooks.test.invalid/v1/hamlet","headers_env":{},"body":{"n":1}}"#,
    )
    .unwrap();

    let data_dir = tmp.path().join(".hamlet-data");
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let mut config = Config::default();
    config.dispatch.enabled = true;
    config.dispatch.url_allowlist = vec!["https://hooks.test.invalid/v1".to_owned()];
    let telemetry = Arc::new(Telemetry::open(&data_dir).unwrap());
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));

    // Restart: the mail plane's startup hook recovers the spool; the
    // dispatch plane's recovery scan picks up the outbox file (no watcher
    // event ever fired for it — it predates attach).
    let dispatcher = hamlet_daemon::watch::Dispatcher::new();
    let client = ScriptedClient::new(vec![http(200, "ok")]);
    let (_router, mut mail_handle) =
        spawn_mail_plane(Arc::clone(&supervisor), Arc::clone(&dispatcher)).unwrap();
    let (plane, _dispatch_handle) = spawn_dispatch_plane(
        Arc::clone(&supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
        &dispatcher,
    )
    .await
    .unwrap();

    assert!(
        wait_until(
            || pending_texts(&supervisor, SCRIBE).len() == 1,
            Duration::from_secs(10)
        )
        .await,
        "downtime mail recovered"
    );
    assert!(
        wait_until(
            || tree
                .courier_dispatch_inbox()
                .join(file_name(&outbox_id))
                .exists(),
            Duration::from_secs(10)
        )
        .await,
        "downtime outbox file recovered"
    );
    assert_eq!(client.call_count(), 1, "exactly one send");
    let inbox = InboxResponse::parse(
        &std::fs::read_to_string(tree.courier_dispatch_inbox().join(file_name(&outbox_id)))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inbox.attempt, 1);
    mail_handle.stop().await;
    let _ = plane;
}

/// Reaction ordering determinism (ADR-0019 cl. 6): a burst of spool writes
/// across recipients dispatches with strictly increasing seq and no
/// duplicates or losses — the (path, seq) contract's observable half.
#[tokio::test]
async fn burst_reactions_sequenced_monotonically() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    for uid in [1001, 1002, 1003] {
        std::fs::create_dir_all(tree.courier_mail_recipient(&name(&uid.to_string()))).unwrap();
    }
    let dispatcher = hamlet_daemon::watch::Dispatcher::new();
    let mut rx = dispatcher.register("collector");
    let mut handle =
        hamlet_daemon::watch::spawn_tree_watcher(tree.clone(), dispatcher, || {}).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Burst: 6 CONFORMING-named files across 3 recipients (2 each),
    // written back-to-back.
    let mut files = Vec::new();
    for uid in [1001, 1002, 1003] {
        for seq in [1u64, 2] {
            let dir = tree.courier_mail_recipient(&name(&uid.to_string()));
            let f = format!("{seq:012}-1002-ab.mail");
            std::fs::write(dir.join(&f), b"x").unwrap();
            files.push((uid, f));
        }
    }

    // Collect until all 6 dispatched (or timeout).
    let mut seen = Vec::new();
    for _ in 0..6 {
        let item = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("reaction within window")
            .unwrap();
        seen.push(item);
    }

    // Monotonic seq, unique.
    let seqs: Vec<u64> = seen.iter().map(|i| i.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq must increase: {seqs:?}"
    );
    // No duplicates, no losses: the (recipient,file) set matches exactly.
    let mut pairs: Vec<(u32, String)> = seen
        .iter()
        .map(|i| match &i.reaction {
            hamlet_core::watch::Reaction::MailSpool { recipient, file } => (
                recipient.as_str().parse::<u32>().unwrap(),
                file.as_str().to_owned(),
            ),
            other => panic!("unexpected reaction {other:?}"),
        })
        .collect();
    pairs.sort();
    let mut expected: Vec<(u32, String)> = files.clone();
    expected.sort();
    assert_eq!(pairs, expected, "exactly one reaction per file");
    handle.stop().await;
}

// ===========================================================================
// H. Freight: audit rule matrix + promotion-read (FakeRuntime)
// ===========================================================================

/// The audit rule table against sneaky listings (pure core): world-writable
/// via ACCESS or DEFAULT other-perms, foreign UIDs, named groups, and the
/// combined case where every rule fires at once — all violations reported
/// in deterministic order (ADR-0021 cl. 1).
#[test]
fn audit_rule_matrix_sneaky_listings() {
    use hamlet_core::freight::{audit, parse_getfacl, AuditPolicy, ViolationKind};

    let policy = AuditPolicy {
        owners: [MAYOR, SCRIBE, HERALD].into_iter().collect(),
        grantees: [MAYOR, SCRIBE, HERALD].into_iter().collect(),
    };

    // Clean baseline: owner slot, other::--- everywhere.
    let clean =
        "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\ngroup::---\nmask::rwx\nother::---\n";
    assert!(audit(&parse_getfacl(clean).unwrap(), &policy).is_empty());

    // World-writable via access other-perms.
    let ww_access = "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::rw-\n";
    assert_kinds(
        &audit(&parse_getfacl(ww_access).unwrap(), &policy),
        &[ViolationKind::WorldWritable],
    );

    // The sneaky one: access other is clean, but the DEFAULT carries write.
    let ww_default = "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::---\ndefault:user::rwx\ndefault:other::rw-\n";
    assert_kinds(
        &audit(&parse_getfacl(ww_default).unwrap(), &policy),
        &[ViolationKind::WorldWritable],
    );

    // Read-only other is fine (the rule is about WRITE).
    let ro_other = "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::r--\n";
    assert!(audit(&parse_getfacl(ro_other).unwrap(), &policy).is_empty());

    // Foreign UID grant (4242 is not on the roster).
    let foreign = "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::---\nuser:4242:rwx\n";
    assert_kinds(
        &audit(&parse_getfacl(foreign).unwrap(), &policy),
        &[ViolationKind::ForeignUid],
    );

    // A grantee on the roster is clean; a slot not in grantees is foreign.
    let roster_grant =
        "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::---\nuser:1002:rwx\n";
    assert!(audit(&parse_getfacl(roster_grant).unwrap(), &policy).is_empty());
    let closed_slot =
        "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::---\nuser:1003:rwx\n";
    assert_kinds(
        &audit(&parse_getfacl(closed_slot).unwrap(), &policy),
        &[ViolationKind::ForeignUid],
    );

    // Named group entries are always a violation (groups aren't charter
    // state) — even read-only.
    let named_group =
        "# file: x\n# owner: 1001\n# group: 1001\nuser::rwx\nother::---\ngroup:501:r--\n";
    assert_kinds(
        &audit(&parse_getfacl(named_group).unwrap(), &policy),
        &[ViolationKind::NamedGroup],
    );

    // Combined: owner-not-slot + world-writable + foreign + group — all
    // four reported. Deterministic order: owner check first, then access
    // entries in listing order (no kind-sorting).
    let combo =
        "# file: x\n# owner: 0\n# group: 0\nuser::rwx\nuser:4242:rwx\ngroup:501:r--\nother::rw-\n";
    let kinds: Vec<_> = audit(&parse_getfacl(combo).unwrap(), &policy)
        .iter()
        .map(|v| v.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            ViolationKind::OwnerNotSlot,
            ViolationKind::ForeignUid,
            ViolationKind::NamedGroup,
            ViolationKind::WorldWritable,
        ]
    );

    // A CR anywhere in the getfacl output is a parse rejection (typed, no
    // panic) — same LF-only discipline as mail.
    let cr_listing = "# file: x\r\n# owner: 1001\nuser::rwx\n";
    assert!(parse_getfacl(cr_listing).is_err());
}

fn assert_kinds(violations: &[hamlet_core::freight::Violation], expected: &[ViolationKind]) {
    let kinds: Vec<_> = violations.iter().map(|v| v.kind).collect();
    assert_eq!(kinds, expected as &[ViolationKind]);
}

/// Daemon-level freight attacks over FakeRuntime: symlink escape shapes are
/// flagged WITHOUT a listing (lstat only — the argv log proves getfacl was
/// never run), and promotion-read reads through strict ACLs via the userns
/// capability while refusing symlinked paths with zero exec calls.
#[tokio::test]
async fn freight_symlink_escape_and_promotion_read_strict_acl() {
    use hamlet_core::freight::ViolationKind;
    use hamlet_daemon::freight::{AuditOutcome, FreightGuard};
    use hamlet_daemon::podman::{ExecOutput, FakeRuntime};

    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    charter.save_atomic(&tree.charter()).unwrap();

    let config = Config::default();
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(tree.clone(), config, spawner, telemetry));
    let runtime = Arc::new(FakeRuntime::new());
    let wake = Arc::new(hamlet_daemon::freight::RecordingWake::new());
    let guard = Arc::new(FreightGuard::new(
        Arc::clone(&supervisor),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        Arc::clone(&wake) as Arc<dyn hamlet_daemon::freight::MayorWake>,
    ));

    let root = tree.courier_freight();
    std::fs::create_dir_all(&root).unwrap();

    // 1. Symlink at the sandbox level → NotADirectory flag, NO getfacl call
    //    (lstat never follows; the listing is never even requested).
    let target = tmp.path().join("outside");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, root.join("evil")).unwrap();
    match guard.audit(&name("evil")).await.unwrap() {
        AuditOutcome::Flagged { violations, .. } => {
            assert_eq!(violations[0].kind, ViolationKind::NotADirectory);
        }
        other => panic!("expected Flagged, got {other:?}"),
    }
    let getfacl_calls: Vec<_> = runtime
        .argv_log()
        .into_iter()
        .filter(|argv| argv.join(" ").contains("getfacl"))
        .collect();
    assert!(
        getfacl_calls.is_empty(),
        "symlink must not trigger a listing"
    );

    // 2. Promotion-read under strict ACLs: the sandbox is owned by the
    //    writer; the daemon reads via `podman unshare cat` — zero ACL
    //    relaxation (the FakeRuntime answers with scripted bytes).
    let sandbox = root.join("report");
    std::fs::create_dir_all(&sandbox).unwrap();
    std::fs::write(sandbox.join("data.bin"), b"artifact-bytes").unwrap();
    runtime.script_exec_for(
        "cat",
        ExecOutput {
            code: 0,
            stdout: b"artifact-bytes".to_vec(),
            stderr: Vec::new(),
        },
    );
    let bytes = guard
        .promotion_read(&name("report"), &name("data.bin"))
        .await
        .unwrap();
    assert_eq!(bytes, b"artifact-bytes");
    // The argv is the userns capability shape (ADR-0021 cl. 6).
    let argvs = runtime.argv_log();
    let cat_argv = argvs
        .iter()
        .find(|argv| argv.join(" ").contains("cat"))
        .expect("unshare cat executed");
    assert_eq!(cat_argv[0], "podman");
    assert_eq!(cat_argv[1], "unshare");

    // 3. Symlinked TARGET file inside a real sandbox → refused, and the
    //    refusal happens BEFORE any exec (zero new calls).
    let before = runtime.argv_log().len();
    std::os::unix::fs::symlink("/etc/passwd", sandbox.join("sneaky")).unwrap();
    assert!(guard
        .promotion_read(&name("report"), &name("sneaky"))
        .await
        .is_err());
    assert_eq!(runtime.argv_log().len(), before, "refusal precedes exec");

    // 4. Symlinked SANDBOX dir → refused likewise.
    std::os::unix::fs::symlink(&target, root.join("evil2")).unwrap();
    assert!(guard
        .promotion_read(&name("evil2"), &name("data.bin"))
        .await
        .is_err());
}

// ===========================================================================
// I. Dispatch: allowlist, secrets, retry schedule, dead-letter, malformed JSON
// ===========================================================================

/// Scripted HTTP seam that stamps each call (retry-gap measurement).
#[derive(Default)]
struct ScriptedClient {
    script: Mutex<VecDeque<Result<WireResponse, String>>>,
    calls: Mutex<Vec<WireRequest>>,
    stamps: Mutex<Vec<Instant>>,
}

impl ScriptedClient {
    fn new(script: Vec<Result<WireResponse, String>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
            stamps: Mutex::new(Vec::new()),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn gaps(&self) -> Vec<Duration> {
        let stamps = self.stamps.lock().unwrap();
        stamps.windows(2).map(|w| w[1] - w[0]).collect()
    }
}

fn http(status: u16, body: &str) -> Result<WireResponse, String> {
    Ok(WireResponse {
        status,
        headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
        body: body.to_owned(),
    })
}

impl HttpClient for ScriptedClient {
    fn execute(
        &self,
        request: WireRequest,
    ) -> Pin<Box<dyn Future<Output = Result<WireResponse, String>> + Send + '_>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request.clone());
            self.stamps.lock().unwrap().push(Instant::now());
            match self.script.lock().unwrap().pop_front() {
                Some(result) => result,
                None => Err("script exhausted".to_owned()),
            }
        })
    }
}

struct DispatchFixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    telemetry: Arc<Telemetry>,
    supervisor: Arc<Supervisor>,
}

fn dispatch_fixture(tweak: impl FnOnce(&mut Config)) -> DispatchFixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    charter.save_atomic(&tree.charter()).unwrap();
    std::fs::write(
        tree.env_file(),
        "HAMLET_DISPATCH_AUTH=test-key-do-not-use\n",
    )
    .unwrap();

    let mut config = Config::default();
    config.dispatch.enabled = true;
    config.dispatch.url_allowlist = vec!["https://hooks.test.invalid/v1".to_owned()];
    config.dispatch.max_attempts = 3;
    config.dispatch.backoff_initial_ms = 100;
    config.dispatch.backoff_max_ms = 400;
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    DispatchFixture {
        _tmp: tmp,
        tree,
        telemetry,
        supervisor,
    }
}

fn write_outbox_raw(tree: &HamletTree, correlation: &str, contents: &str) -> CorrelationId {
    let id = CorrelationId::parse(correlation).unwrap();
    std::fs::create_dir_all(tree.courier_dispatch_outbox()).unwrap();
    std::fs::write(
        tree.courier_dispatch_outbox().join(file_name(&id)),
        contents,
    )
    .unwrap();
    id
}

fn outbox_request(url: &str, correlation: &str) -> String {
    OutboxRequest {
        body: serde_json::json!({"event": "adversarial"}),
        correlation: CorrelationId::parse(correlation).unwrap(),
        dispatch_version: DISPATCH_VERSION,
        headers_env: BTreeMap::from([(
            "X-Hamlet-Auth".to_owned(),
            "HAMLET_DISPATCH_AUTH".to_owned(),
        )]),
        method: Method::Post,
        url: url.to_owned(),
    }
    .render()
    .unwrap()
}

/// The allowlist is an EXACT authority + byte-prefix path check (ADR-0020
/// cl. 3): subdomains, parent domains, port changes, and prefix escapes all
/// fail; case-insensitive host matching works; empty list denies all.
#[test]
fn allowlist_matrix_exact_authority_and_prefix() {
    let al = UrlAllowlist::parse(&["https://hooks.test.invalid/v1".to_owned()]).unwrap();

    // Exact authority + prefix.
    assert!(al.allows("https://hooks.test.invalid/v1"));
    assert!(al.allows("https://hooks.test.invalid/v1/x/y"));
    // Case-insensitive host.
    assert!(al.allows("https://HOOKS.test.INVALID/v1/z"));

    // Subdomain of the allowed host: different authority → denied.
    assert!(!al.allows("https://x.hooks.test.invalid/v1"));
    // Parent domain: denied.
    assert!(!al.allows("https://test.invalid/v1"));
    // Port change: denied (exact port).
    assert!(!al.allows("https://hooks.test.invalid:8443/v1"));
    // ADR-0020 cl. 3: path match is BYTE-prefix (not segment-boundary):
    // /v1evil/x starts with /v1 and IS allowed. Documented behavior —
    // operators must pick prefixes with that in mind (see QA note).
    assert!(al.allows("https://hooks.test.invalid/v1evil/x"));

    // http:// is rejected at CONFIG-validation time (ADR-0020 cl. 3,
    // fail-closed): the entry never even reaches the matcher.
    assert!(UrlAllowlist::parse(&["http://insecure.test.invalid".to_owned()]).is_err());

    // Empty list denies everything.
    let empty = UrlAllowlist::parse(&[]).unwrap();
    assert!(empty.is_empty());
    assert!(!empty.allows("https://anything.example/x"));
}

/// Secret hygiene end-to-end (ADR-0020 cl. 6; CONVENTIONS §6): the secret
/// is resolved from `.env` by key at send time and must NEVER appear in any
/// tree artifact or sled trace — while the wire request DID carry it.
#[tokio::test]
async fn secrets_never_in_artifacts_or_sled() {
    const SECRET: &str = "top-secret-value-qa04";
    let fx = dispatch_fixture(|_| {});
    std::fs::write(
        fx.tree.env_file(),
        format!("HAMLET_DISPATCH_AUTH={SECRET}\n"),
    )
    .unwrap();

    let client = ScriptedClient::new(vec![http(200, "{\"ok\":true}")]);
    let plane = Arc::new(DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    ));
    let id = write_outbox_raw(
        &fx.tree,
        "d-1-1001.0",
        &outbox_request("https://hooks.test.invalid/v1/hamlet", "d-1-1001.0"),
    );

    assert_eq!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::Delivered {
            attempt: 1,
            status: 200
        }
    );

    // The wire request carried the resolved secret (the seam recorded it).
    let calls = client.calls.lock().unwrap().clone();
    assert_eq!(
        calls[0].headers.get("X-Hamlet-Auth").map(String::as_str),
        Some(SECRET)
    );

    // No artifact under courier/dispatch contains the secret.
    for sub in ["inbox", "outbox", "dead"] {
        let dir = fx.tree.courier().join("dispatch").join(sub);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let bytes = std::fs::read(&path).unwrap();
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(
                        !text.contains(SECRET),
                        "secret leaked into {sub}/{}",
                        path.file_name().unwrap().to_string_lossy()
                    );
                }
            }
        }
    }

    // No sled trace carries the secret (any key).
    for uid in [0u32, SCRIBE] {
        for commission in [0u64, COMMISSION] {
            let traces = fx
                .telemetry
                .recent_traces(AgentKey { commission, uid }, 100)
                .unwrap();
            for t in &traces {
                assert!(
                    !t.detail.contains(SECRET),
                    "secret leaked into sled: {}",
                    t.detail
                );
            }
        }
    }
}

/// Response headers that carry secrets are REDACTED from the inbox file
/// (ADR-0020 cl. 2): authorization/cookie/set-cookie/x-api-key never
/// stored; other headers lowercased and kept.
#[tokio::test]
async fn response_header_redaction_applied() {
    let fx = dispatch_fixture(|_| {});
    let client = ScriptedClient::new(vec![Ok(WireResponse {
        status: 200,
        headers: BTreeMap::from([
            (
                "Set-Cookie".to_owned(),
                "session=SECRET-COOKIE-VAL".to_owned(),
            ),
            ("X-Api-Key".to_owned(), "leak".to_owned()),
            ("Authorization".to_owned(), "Bearer leak".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("X-RateLimit-Remaining".to_owned(), "42".to_owned()),
        ]),
        body: "{}".to_owned(),
    })]);
    let plane = Arc::new(DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    ));
    let id = write_outbox_raw(
        &fx.tree,
        "d-2-1001.0",
        &outbox_request("https://hooks.test.invalid/v1/hamlet", "d-2-1001.0"),
    );
    plane.process(&id).await.unwrap();

    let inbox = InboxResponse::parse(
        &std::fs::read_to_string(fx.tree.courier_dispatch_inbox().join(file_name(&id))).unwrap(),
    )
    .unwrap();
    match &inbox.outcome {
        Outcome::Http { headers, .. } => {
            assert!(
                headers.get("set-cookie").is_none(),
                "cookie must be redacted"
            );
            assert!(headers.get("x-api-key").is_none());
            assert!(headers.get("authorization").is_none());
            // Non-sensitive headers survive, lowercased.
            assert_eq!(
                headers.get("content-type").map(String::as_str),
                Some("application/json")
            );
            assert_eq!(
                headers.get("x-ratelimit-remaining").map(String::as_str),
                Some("42")
            );
        }
        other => panic!("expected http outcome, got {other:?}"),
    }
    // And the raw secret cookie value is nowhere in the file bytes.
    let raw =
        std::fs::read_to_string(fx.tree.courier_dispatch_inbox().join(file_name(&id))).unwrap();
    assert!(!raw.contains("SECRET-COOKIE-VAL"));
}

/// The retry schedule is a PURE FUNCTION (ADR-0020 cl. 4): the core policy
/// yields exact tick values — no jitter, deterministic.
#[test]
fn retry_schedule_is_a_pure_function() {
    let p = RetryPolicy {
        max_attempts: 4,
        initial_ms: 5,
        max_ms: 100,
    };
    assert_eq!(p.schedule(), vec![5, 10, 20]);

    let p = RetryPolicy {
        max_attempts: 3,
        initial_ms: 250,
        max_ms: 900,
    };
    assert_eq!(p.schedule(), vec![250, 500]);

    // The cap binds from the second tick on.
    let p = RetryPolicy {
        max_attempts: 5,
        initial_ms: 60,
        max_ms: 100,
    };
    assert_eq!(p.schedule(), vec![60, 100, 100, 100]);

    // One attempt: no delays at all.
    let p = RetryPolicy {
        max_attempts: 1,
        initial_ms: 5,
        max_ms: 100,
    };
    assert!(p.schedule().is_empty());
}

/// Wall-clock retry honors the same schedule (no jitter): transport errors
/// on attempts 1 and 2 sleep ≥ initial and ≥ min(2·initial, max) before the
/// next attempt; exhaustion dead-letters after exactly max_attempts calls.
#[tokio::test]
async fn retry_wall_clock_gaps_and_dead_letter_budget() {
    let fx = dispatch_fixture(|c| {
        c.dispatch.backoff_initial_ms = 100;
        c.dispatch.backoff_max_ms = 400;
    });
    let client = ScriptedClient::new(vec![
        Err("net down".into()),
        Err("net down".into()),
        http(200, "ok"),
    ]);
    let plane = Arc::new(DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    ));
    let id = write_outbox_raw(
        &fx.tree,
        "d-3-1001.0",
        &outbox_request("https://hooks.test.invalid/v1/hamlet", "d-3-1001.0"),
    );

    assert_eq!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::Delivered {
            attempt: 3,
            status: 200
        }
    );
    let gaps = client.gaps();
    assert_eq!(gaps.len(), 2);
    assert!(
        gaps[0].as_millis() >= 95,
        "gap0 {:?} below initial",
        gaps[0]
    );
    assert!(
        gaps[1].as_millis() >= 195,
        "gap1 {:?} below min(2*initial,max)",
        gaps[1]
    );

    // Exhaustion: three transport errors → dead-letter after exactly 3 calls.
    let client = ScriptedClient::new(vec![
        Err("net down".into()),
        Err("net down".into()),
        Err("net down".into()),
    ]);
    let plane = Arc::new(DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    ));
    let original = outbox_request("https://hooks.test.invalid/v1/hamlet", "d-4-1001.0");
    let id = write_outbox_raw(&fx.tree, "d-4-1001.0", &original);
    assert!(matches!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::DeadLettered { attempt: 3, .. }
    ));
    assert_eq!(client.call_count(), 3, "exactly max_attempts network calls");
    // Dead-letter preserves the original bytes.
    let dead =
        std::fs::read_to_string(fx.tree.courier_dispatch_dead().join(file_name(&id))).unwrap();
    assert_eq!(dead, original);
    // Final error inbox records attempt 3.
    let inbox = InboxResponse::parse(
        &std::fs::read_to_string(fx.tree.courier_dispatch_inbox().join(file_name(&id))).unwrap(),
    )
    .unwrap();
    assert_eq!(inbox.attempt, 3);
}

/// Malformed outbox files (binary garbage, empty, wrong version, missing
/// url) each dead-letter with a PRECISE error inbox — zero network calls,
/// no panic — and the plane STAYS ALIVE for the next valid file.
#[tokio::test]
async fn malformed_json_matrix_precise_error_and_plane_stays_alive() {
    let fx = dispatch_fixture(|_| {});

    let cases: Vec<(&str, String)> = vec![
        ("binary-garbage", "\u{0}\u{ff}\u{fe}not-json".into()),
        ("empty-file", String::new()),
        (
            "wrong-version",
            r#"{"correlation":"d-6-1001.0","dispatch_version":99,"method":"post","url":"https://hooks.test.invalid/v1/x","headers_env":{},"body":{}}"#.into(),
        ),
        (
            "missing-url",
            r#"{"correlation":"d-7-1001.0","dispatch_version":1,"method":"post","headers_env":{},"body":{}}"#.into(),
        ),
    ];

    for (i, (label, contents)) in cases.iter().enumerate() {
        let corr = format!("d-{n}-1001.0", n = 5 + i);
        let client = ScriptedClient::new(vec![http(200, "ok")]);
        let plane = Arc::new(DispatchPlane::new(
            Arc::clone(&fx.supervisor),
            Arc::clone(&client) as Arc<dyn HttpClient>,
        ));
        let id = write_outbox_raw(&fx.tree, &corr, contents);

        assert!(
            matches!(
                plane.process(&id).await.unwrap(),
                DispatchOutcome::DeadLettered { attempt: 1, .. }
            ),
            "case {label}"
        );
        assert_eq!(client.call_count(), 0, "{label}: zero network calls");
        // Precise error inbox (attempt 1) + dead-letter with original bytes.
        let inbox = InboxResponse::parse(
            &std::fs::read_to_string(fx.tree.courier_dispatch_inbox().join(file_name(&id)))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(inbox.attempt, 1);
        assert!(!matches!(inbox.outcome, Outcome::Http { .. }), "{label}");
        let dead =
            std::fs::read_to_string(fx.tree.courier_dispatch_dead().join(file_name(&id))).unwrap();
        assert_eq!(dead, *contents, "{label}: dead-letter preserves bytes");
    }

    // Plane liveness: the next VALID file still processes end-to-end.
    let client = ScriptedClient::new(vec![http(200, "ok")]);
    let plane = Arc::new(DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    ));
    let id = write_outbox_raw(
        &fx.tree,
        "d-9-1001.0",
        &outbox_request("https://hooks.test.invalid/v1/hamlet", "d-9-1001.0"),
    );
    assert_eq!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::Delivered {
            attempt: 1,
            status: 200
        }
    );
}

// ===========================================================================
// J. Inbound webhooks: auth, oversize, traversal, content-type smuggling
// ===========================================================================

const WEBHOOK_SECRET: &str = "qa-webhook-secret";

struct WebFixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    service: Arc<WebhookService>,
}

fn web_fixture(tweak: impl FnOnce(&mut Config)) -> WebFixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    charter.save_atomic(&tree.charter()).unwrap();
    std::fs::write(
        tree.env_file(),
        format!("HAMLET_WEBHOOK_AUTH={WEBHOOK_SECRET}\n"),
    )
    .unwrap();

    let mut config = Config {
        web_listen_addr: "127.0.0.1:0".to_owned(),
        ..Default::default()
    };
    config.webhooks.enabled = true;
    config.webhooks.max_body_bytes = 4096;
    config.webhooks.routes = BTreeMap::from([
        (
            "deploy".to_owned(),
            WebhookRoute {
                dispatch_inbox: true,
                mail_to: vec![SCRIBE],
            },
        ),
        (
            "audit".to_owned(),
            WebhookRoute {
                dispatch_inbox: false,
                mail_to: vec![SCRIBE, HERALD],
            },
        ),
    ]);
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
    let wake = Arc::new(RecordingWebhookWake::new());
    let service = Arc::new(WebhookService::new(
        Arc::clone(&supervisor),
        router,
        Arc::clone(&wake) as Arc<dyn hamlet_daemon::web::WebhookWake>,
    ));
    WebFixture {
        _tmp: tmp,
        tree,
        service,
    }
}

/// Every visible file in the village tree, sorted — rejection tests assert
/// this is unchanged (nothing touches disk).
fn tree_listing(tree: &HamletTree) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    let mut out = Vec::new();
    walk(tree.root(), tree.root(), &mut out);
    out.sort();
    out
}

/// Raw HTTP over a plain socket (reqwest normalizes paths and headers; the
/// smuggling tests need both under our control). `Connection: close` so the
/// response ends at EOF.
fn raw_http(port: u16, path: &str, extra_headers: &str, body: &[u8]) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Hamlet-Auth: {WEBHOOK_SECRET}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, text)
}

/// Bad auth: missing / wrong / prefix / empty secret all get the SAME 401
/// class with a fixed reason token — constant-time compare, no early exit,
/// tree untouched (ADR-0022 cl. 1).
#[tokio::test]
async fn bad_auth_variants_401_tree_untouched() {
    let fx = web_fixture(|_| {});
    let before = tree_listing(&fx.tree);

    // Service level: every bad-auth shape is the same rejection class.
    for auth in [None, Some("wrong"), Some("qa-webhook-sec"), Some("")] {
        let result = fx.service.accept("deploy", auth, br#"{"x":1}"#);
        assert!(
            matches!(result, Err(hamlet_daemon::web::Rejection::BadAuth)),
            "{auth:?}"
        );
    }

    // HTTP level: 401 + fixed token, no echo of the presented value.
    let handle = spawn_web_server(Arc::clone(&fx.service))
        .await
        .expect("binds");
    let base = format!("http://{}", handle.local_addr());
    for auth in [None, Some("wrong"), Some("qa-webhook-sec")] {
        let mut request = reqwest::Client::new()
            .post(format!("{base}/webhook/deploy"))
            .body("{\"x\":1}".to_owned());
        if let Some(secret) = auth {
            request = request.header(AUTH_HEADER, secret);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401);
        let body = response.text().await.unwrap();
        assert_eq!(body, r#"{"error":"bad-auth"}"#, "fixed token, no echo");
    }

    // Tree untouched by every rejection (checked BEFORE the control case,
    // which legitimately writes artifacts).
    assert_eq!(
        tree_listing(&fx.tree),
        before,
        "rejections must not touch the tree"
    );

    // Control: the GOOD secret accepts.
    let response = reqwest::Client::new()
        .post(format!("{base}/webhook/deploy"))
        .header(AUTH_HEADER, WEBHOOK_SECRET)
        .body("{\"x\":1}".to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202, "good secret must be accepted");
    handle.stop().await;
}

/// Fail-closed: with the auth key MISSING from `.env`, even a request that
/// carries the right-looking header gets 503 — never a default secret.
#[tokio::test]
async fn auth_env_missing_fails_closed() {
    let fx = web_fixture(|_| {});
    std::fs::write(fx.tree.env_file(), "# no webhook key\n").unwrap();

    let result = fx
        .service
        .accept("deploy", Some(WEBHOOK_SECRET), br#"{"x":1}"#);
    assert!(matches!(
        result,
        Err(hamlet_daemon::web::Rejection::AuthUnavailable)
    ));

    let handle = spawn_web_server(Arc::clone(&fx.service))
        .await
        .expect("binds");
    let base = format!("http://{}", handle.local_addr());
    let response = reqwest::Client::new()
        .post(format!("{base}/webhook/deploy"))
        .header(AUTH_HEADER, WEBHOOK_SECRET)
        .body("{\"x\":1}".to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert_eq!(body, r#"{"error":"auth-unavailable"}"#);
    handle.stop().await;
}

/// Oversize bodies get 413 at BOTH layers (ADR-0022 cl. 2): the tower
/// `RequestBodyLimitLayer` in hyper before the handler even runs, and the
/// service-level check for direct callers. Tree untouched either way.
#[tokio::test]
async fn oversize_body_413_both_layers() {
    let fx = web_fixture(|c| c.webhooks.max_body_bytes = 256);
    let before = tree_listing(&fx.tree);

    // Service level: max+1 bytes → TooLarge.
    let big = vec![b'x'; 257];
    assert!(matches!(
        fx.service.accept("deploy", Some(WEBHOOK_SECRET), &big),
        Err(hamlet_daemon::web::Rejection::TooLarge)
    ));

    // HTTP level: hyper's body limit rejects before the handler runs.
    let handle = spawn_web_server(Arc::clone(&fx.service))
        .await
        .expect("binds");
    let base = format!("http://{}", handle.local_addr());
    let response = reqwest::Client::new()
        .post(format!("{base}/webhook/deploy"))
        .header(AUTH_HEADER, WEBHOOK_SECRET)
        .body(vec![b'x'; 300])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);

    // Tree untouched by both rejection layers (checked BEFORE the control
    // case, which legitimately writes artifacts).
    assert_eq!(
        tree_listing(&fx.tree),
        before,
        "413 must not touch the tree"
    );

    // Control: an EXACT-cap body still passes (the bound is inclusive).
    let json: Vec<u8> = format!("{{\"pad\":\"{}\"}}", "y".repeat(246)).into();
    assert_eq!(json.len(), 256, "body is exactly at the cap");
    let result = fx.service.accept("deploy", Some(WEBHOOK_SECRET), &json);
    assert!(result.is_ok(), "exact-cap body accepted");
    handle.stop().await;
}

/// Path-traversal topics are ONE indistinguishable class (ADR-0022 cl. 3):
/// `..`, encoded variants, absolute paths, and foreign charset all get the
/// same 404 fixed token — the sanitizer runs at the boundary, so the raw
/// topic never reaches a path, a subject, or the response body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traversal_topics_404_sanitizer_clean() {
    let fx = web_fixture(|_| {});
    let before = tree_listing(&fx.tree);

    // Service level: every traversal shape → BadTopic (same class as an
    // unknown topic — one indistinguishable 404).
    for topic in [
        "..",
        "../x",
        "%2e%2e",
        "/etc/passwd",
        "a/b",
        "\u{0}evil",
        "..\u{ffff}",
    ] {
        let result = fx
            .service
            .accept(topic, Some(WEBHOOK_SECRET), br#"{"x":1}"#);
        assert!(
            matches!(result, Err(hamlet_daemon::web::Rejection::BadTopic)),
            "{topic:?}"
        );
    }
    // Unknown-but-clean topic: same class.
    let result = fx
        .service
        .accept("ghost", Some(WEBHOOK_SECRET), br#"{"x":1}"#);
    assert!(matches!(
        result,
        Err(hamlet_daemon::web::Rejection::UnknownTopic)
    ));

    // HTTP level with raw (un-normalized) encoded paths: 4xx, never 2xx/5xx.
    let handle = spawn_web_server(Arc::clone(&fx.service))
        .await
        .expect("binds");
    let base = format!("http://{}", handle.local_addr());
    let port = base.rsplit_once(':').unwrap().1.parse::<u16>().unwrap();
    for path in [
        "/webhook/%2e%2e",
        "/webhook/..",
        "/webhook/..%2f..%2fetc%2fpasswd",
    ] {
        let (status, response) = raw_http(port, path, "", br#"{"x":1}"#);
        assert!(
            matches!(status, 400 | 404),
            "{path} → {status}: traversal must be a rejection class"
        );
        assert!(!response.contains("etc"), "no path echo in the response");
    }

    // Tree untouched.
    assert_eq!(
        tree_listing(&fx.tree),
        before,
        "traversal rejections must not touch the tree"
    );
    handle.stop().await;
}

/// Content-type smuggling: the handler validates the BODY only — the
/// `Content-Type` header is never trusted (ADR-0022 cl. 4). JSON with no
/// content type → accepted; text/plain + JSON → accepted; application/json
/// + garbage → 400 malformed-json.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_type_smuggling_body_is_truth() {
    let fx = web_fixture(|_| {});
    let handle = spawn_web_server(Arc::clone(&fx.service))
        .await
        .expect("binds");
    let base = format!("http://{}", handle.local_addr());
    let port = base.rsplit_once(':').unwrap().1.parse::<u16>().unwrap();

    // 1. No Content-Type header at all + JSON body → accepted (202).
    let (status, _) = raw_http(port, "/webhook/deploy", "", br#"{"x":1}"#);
    assert_eq!(status, 202, "body parsed without a content-type header");

    // 2. text/plain + JSON body → still accepted (header not trusted).
    let (status, _) = raw_http(
        port,
        "/webhook/deploy",
        "Content-Type: text/plain\r\n",
        br#"{"x":1}"#,
    );
    assert_eq!(status, 202, "text/plain header must not block JSON bodies");

    // 3. application/json + garbage body → 400 malformed-json (the BODY is
    //    what failed, the header was irrelevant).
    let (status, response) = raw_http(
        port,
        "/webhook/deploy",
        "Content-Type: application/json\r\n",
        b"not-json",
    );
    assert_eq!(status, 400);
    assert!(
        response.contains(r#"{"error":"malformed-json"}"#),
        "{response}"
    );

    handle.stop().await;
}

/// Every rejection artifact is sanitizer-clean: the response body carries
/// ONLY the fixed reason token — never the topic, body, or any input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejection_artifacts_never_echo_input() {
    let fx = web_fixture(|_| {});
    let handle = spawn_web_server(Arc::clone(&fx.service))
        .await
        .expect("binds");
    let base = format!("http://{}", handle.local_addr());

    // A topic-laden rejection (raw socket: reqwest would normalize the
    // path): the topic string must not appear in the response body.
    let port = base.rsplit_once(':').unwrap().1.parse::<u16>().unwrap();
    let (status, response) = raw_http(port, "/webhook/%2e%2e", "", br#"{"x":1}"#);
    assert_eq!(status, 404);
    let body = response
        .lines()
        .last()
        .map(str::to_owned)
        .unwrap_or_default();
    assert_eq!(body, r#"{"error":"unknown-topic"}"#);
    assert!(!response.contains(".."), "topic must not be echoed");

    // A malformed-body rejection: the garbage must not be echoed back.
    let garbage = b"\xff\xfe{not-json-at-all";
    let response = reqwest::Client::new()
        .post(format!("{base}/webhook/deploy"))
        .header(AUTH_HEADER, WEBHOOK_SECRET)
        .body(garbage.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body = response.text().await.unwrap();
    assert_eq!(body, r#"{"error":"malformed-json"}"#);

    handle.stop().await;
}

// ===========================================================================
// I-bis. Regression: the adversarial suite must attack the ENFORCEMENT path,
// not just the pure matcher (QA04-10)
//
// Mutation testing proved that deleting the daemon's allowlist call
// (`dispatch.rs`, `if !allowlist.allows(&request.url)`) left this file 27/27
// GREEN, because `allowlist_matrix_exact_authority_and_prefix` exercises only
// `hamlet_core::dispatch::UrlAllowlist` in isolation. `tests/dispatch.rs` did
// catch it — but the suite whose stated remit is "non-allowlisted URL refused"
// did not. These tests pin the refusal at the plane boundary so the gap cannot
// reopen.
// ===========================================================================

/// A URL outside the configured allowlist is refused by the PLANE, with ZERO
/// network attempts, and leaves a dead-letter that names only the authority
/// (SPEC §8.4; ADR-0020 cl. 3).
#[tokio::test]
async fn disallowed_url_is_refused_before_any_network_attempt() {
    let fx = dispatch_fixture(|_| {}); // allowlist: https://hooks.test.invalid/v1
    let client = ScriptedClient::new(vec![http(200, "should never be reached")]);
    let plane = DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    );

    // Same host, different port; a wholly foreign host; and a lookalike
    // subdomain. All three must die before any bytes leave the process.
    for (url, correlation) in [
        ("https://hooks.test.invalid:8443/v1/hamlet", "d-11-1001.0"),
        ("https://evil.example/v1/steal", "d-12-1001.0"),
        ("https://x.hooks.test.invalid/v1/hamlet", "d-13-1001.0"),
    ] {
        let id = write_outbox_raw(&fx.tree, correlation, &outbox_request(url, correlation));
        assert!(
            matches!(
                plane.process(&id).await.unwrap(),
                DispatchOutcome::DeadLettered { .. }
            ),
            "{url} must be dead-lettered"
        );
    }

    assert_eq!(
        client.call_count(),
        0,
        "a non-allowlisted URL must never reach the HTTP seam"
    );
    // Dead-letter preserves the original bytes; the inbox error names the
    // authority but NOT the secret-bearing header/body.
    let dead = std::fs::read_to_string(
        fx.tree
            .courier_dispatch_dead()
            .join(file_name(&CorrelationId::parse("d-12-1001.0").unwrap())),
    )
    .unwrap();
    assert!(dead.contains("evil.example"));
    assert!(!dead.contains("test-key-do-not-use"));
}

/// ADR-0020 cl. 4 / ADR-0023: the delay the plane actually sleeps must equal
/// the deterministic, jitter-free schedule. Mutation testing added
/// `ms + (attempt % 7) * 3` jitter to the sleep in `dispatch.rs` and the
/// WHOLE WORKSPACE (36 test binaries) stayed green — the only schedule
/// assertions were on the pure `RetryPolicy::schedule()` and a LOWER-BOUND
/// wall-clock check. This pins the upper bound: no gap may exceed the
/// scheduled delay beyond scheduler slack.
#[tokio::test]
async fn backoff_gaps_never_exceed_the_deterministic_schedule() {
    const INITIAL: u64 = 400;
    const MAX: u64 = 1_000;
    let fx = dispatch_fixture(|c| {
        c.dispatch.backoff_initial_ms = INITIAL;
        c.dispatch.backoff_max_ms = MAX;
        c.dispatch.max_attempts = 4;
    });
    let client = ScriptedClient::new(vec![
        Err("net down".into()),
        Err("net down".into()),
        Err("net down".into()),
        http(200, "ok"),
    ]);
    let plane = DispatchPlane::new(
        Arc::clone(&fx.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
    );
    let id = write_outbox_raw(
        &fx.tree,
        "d-21-1001.0",
        &outbox_request("https://hooks.test.invalid/v1/hamlet", "d-21-1001.0"),
    );
    assert!(matches!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::Delivered { attempt: 4, .. }
    ));

    // The schedule is the single source of truth for the expected delays.
    let expected = RetryPolicy {
        max_attempts: 4,
        initial_ms: INITIAL,
        max_ms: MAX,
    }
    .schedule();
    assert_eq!(expected.len(), 3, "three sleeps before the fourth attempt");

    let gaps = client.gaps();
    assert_eq!(gaps.len(), 3);
    for (i, (gap, want)) in gaps.iter().zip(&expected).enumerate() {
        let got = gap.as_millis() as u64;
        // Lower bound: the delay really happened (scheduler may fire early by
        // a hair, hence the small grace).
        assert!(got + 5 >= *want, "gap{i} {got}ms below scheduled {want}ms");
        // Upper bound: NO jitter (ADR-0020 cl. 4). Slack covers timer
        // granularity and load, and is far below any real backoff policy
        // change.
        assert!(
            got <= want + 60,
            "gap{i} {got}ms EXCEEDS scheduled {want}ms — jittered backoff? \
             (ADR-0020 cl. 4 requires the jitter-free schedule)"
        );
    }
}
