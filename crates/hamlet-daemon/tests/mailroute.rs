//! Mail spool routing integration tests (Sprint 04 step 02; SPEC §8.1,
//! ADR-0019 — CONVENTIONS §9 doctrine: tempdir tree, no podman).
//!
//! Real [`MailRouter`] + real Sprint 03 [`QueueStore`] persistence against
//! a tempdir village. Coverage: deliver → enqueue → `.read/` ack, typed
//! bounces (no slot, crashed session, overflow, malformed, To-mismatch),
//! daemon-asserted send API, restart-mid-delivery recovery with **zero
//! duplicates**, watcher-plane end-to-end (recovery + events), and the
//! 1k-write storm (all delivered, per-recipient FIFO, bounded channels).

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::Config;
use hamlet_core::mail::{
    is_incoming, parse_spool_file_name, spool_file_name, Mail, MailHeader, MailSender,
};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{QueuedKind, SessionEnvelope, SessionId, SessionStatus};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::title_stub;
use hamlet_daemon::mailroute::{spawn_mail_plane, MailActor, MailRouter, RouteOutcome};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{AgentKey, Telemetry};
use hamlet_daemon::watch::Dispatcher;

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;
const SCRIBE: u32 = 1001;
const HERALD: u32 = 1002;

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

fn agent_key(uid: u32) -> AgentKey {
    AgentKey {
        commission: COMMISSION,
        uid,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    telemetry: Arc<Telemetry>,
    supervisor: Arc<Supervisor>,
    router: Arc<MailRouter>,
}

fn fixture(tweak: impl FnOnce(&mut Config)) -> Fixture {
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
    for uid in [SCRIBE, HERALD] {
        fs::create_dir_all(tree.parcel(uid)).unwrap();
        fs::write(
            tree.parcel_title(uid),
            title_stub(
                &name(&format!("agent{uid}")),
                uid,
                COMMISSION,
                1_700_000_000,
            ),
        )
        .unwrap();
    }

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
    Fixture {
        _tmp: tmp,
        tree,
        telemetry,
        supervisor,
        router,
    }
}

/// Render a canonical mail for spool writes.
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

/// Direct (test-writer) spool placement with the conforming name grammar.
fn write_spool(tree: &HamletTree, recipient: u32, seq: u64, writer: u32, text: &str) -> SafeName {
    let dir = tree.courier_mail_recipient(&name(&recipient.to_string()));
    fs::create_dir_all(&dir).unwrap();
    let file = spool_file_name(seq, writer, &format!("{seq:x}")).unwrap();
    fs::write(dir.join(&file), text).unwrap();
    name(&file)
}

fn spool_names(tree: &HamletTree, recipient: u32) -> Vec<String> {
    let dir = tree.courier_mail_recipient(&name(&recipient.to_string()));
    let mut out: Vec<String> = fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| is_incoming(n))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn read_names(tree: &HamletTree, recipient: u32) -> Vec<String> {
    let dir = tree.courier_mail_read(&name(&recipient.to_string()));
    let mut out: Vec<String> = fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Routing: delivery + ack
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_delivers_enqueues_and_acks() {
    let fx = fixture(|_| {});
    let text = mail_text("1002", SCRIBE, "Report ready", "Findings inside.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);

    let outcome = fx.router.route(&name("1001"), &file).unwrap();
    let RouteOutcome::Delivered { uid, correlation } = outcome else {
        panic!("expected delivery, got {outcome:?}");
    };
    assert_eq!(uid, SCRIBE);
    assert_eq!(correlation.as_str(), "m-1-1001.0");

    // Spool consumed; ack visible in .read/ only (never rescanned).
    assert!(spool_names(&fx.tree, SCRIBE).is_empty());
    assert_eq!(read_names(&fx.tree, SCRIBE), vec![file.as_str()]);

    // Envelope persisted in the Sprint 03 queue, canonical render inside
    // (parse→render normalizes the trailing newline).
    let pending = fx.supervisor.queues().pending(SCRIBE).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].queued_kind(), Some(QueuedKind::Mail));
    match &pending[0] {
        SessionEnvelope::Mail { text: queued, .. } => {
            assert_eq!(queued, &Mail::parse(&text).unwrap().render())
        }
        other => panic!("unexpected {other:?}"),
    }

    // Sled trace recorded (telemetry only — the tree carries the state).
    let traces = fx.telemetry.recent_traces(agent_key(SCRIBE), 16).unwrap();
    assert!(
        traces
            .iter()
            .any(|t| t.event == "mail-routed" && t.detail.contains(file.as_str())),
        "{traces:?}"
    );
}

#[tokio::test]
async fn route_is_idempotent_on_vanished_file() {
    let fx = fixture(|_| {});
    let text = mail_text("1002", SCRIBE, "Gone", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);
    let first = fx.router.route(&name("1001"), &file).unwrap();
    assert!(matches!(first, RouteOutcome::Delivered { .. }));
    // A duplicate hint (event replay, second scan) is a cheap no-op.
    let second = fx.router.route(&name("1001"), &file).unwrap();
    assert_eq!(second, RouteOutcome::AlreadyConsumed);
    assert_eq!(
        pending_texts(&fx.supervisor, SCRIBE).len(),
        1,
        "no duplicate enqueue"
    );
}

#[tokio::test]
async fn recipient_scan_preserves_writer_fifo() {
    let fx = fixture(|_| {});
    // Arrive out of order on disk; the sorted scan restores FIFO.
    for seq in [3u64, 1, 2] {
        let text = mail_text("1002", SCRIBE, &format!("msg-{seq}"), "body");
        write_spool(&fx.tree, SCRIBE, seq, HERALD, &text);
    }
    let report = fx.router.route_recipient(&name("1001")).unwrap();
    assert_eq!(
        (report.scanned, report.delivered, report.bounced),
        (3, 3, 0)
    );
    let subjects: Vec<String> = pending_texts(&fx.supervisor, SCRIBE)
        .iter()
        .map(|t| t.lines().nth(2).unwrap().to_owned())
        .collect();
    assert_eq!(
        subjects,
        ["Subject: msg-1", "Subject: msg-2", "Subject: msg-3"]
    );
}

// ---------------------------------------------------------------------------
// Bounces (ADR-0019 cl. 4)
// ---------------------------------------------------------------------------

/// Route the single bounce mail that landed in `uid`'s spool and return
/// its parsed form.
fn route_bounce(fx: &Fixture, uid: u32) -> Mail {
    let names = spool_names(&fx.tree, uid);
    assert_eq!(
        names.len(),
        1,
        "exactly one bounce in {uid}'s spool: {names:?}"
    );
    let outcome = fx
        .router
        .route(&name(&uid.to_string()), &name(&names[0]))
        .unwrap();
    assert!(
        matches!(outcome, RouteOutcome::Delivered { .. }),
        "{outcome:?}"
    );
    let text = &pending_texts(&fx.supervisor, uid)[0];
    Mail::parse(text).unwrap()
}

#[tokio::test]
async fn bounce_no_slot() {
    let fx = fixture(|_| {});
    // 1005 has no charter slot; mail addressed to it must bounce to the
    // sender (SCRIBE) and be consumed.
    fs::create_dir_all(fx.tree.courier_mail_recipient(&name("1005"))).unwrap();
    let text = mail_text("1001", 1005, "Are you there", "Body.");
    let file = write_spool(&fx.tree, 1005, 1, SCRIBE, &text);

    let outcome = fx.router.route(&name("1005"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { reason, to } => {
            assert_eq!(reason.to_string(), "no slot allocated for UID 1005");
            assert_eq!(to, Some(SCRIBE));
        }
        other => panic!("expected bounce, got {other:?}"),
    }
    assert!(spool_names(&fx.tree, 1005).is_empty());
    assert_eq!(read_names(&fx.tree, 1005), vec![file.as_str()]);

    let bounce = route_bounce(&fx, SCRIBE);
    assert_eq!(bounce.header.from, MailSender::Name(name("system")));
    assert_eq!(bounce.header.to, SCRIBE);
    assert_eq!(bounce.header.subject, "Bounce: Are you there");
    assert!(bounce.body.contains("no slot allocated for UID 1005"));
    assert!(bounce.body.contains("Subject: Are you there"));
    assert!(!bounce.body.contains("Body."), "original body never copied");

    let traces = fx.telemetry.recent_traces(agent_key(SCRIBE), 16).unwrap();
    assert!(
        traces.iter().any(|t| t.event == "mail-bounced"),
        "{traces:?}"
    );
}

#[tokio::test]
async fn bounce_crashed_session() {
    let fx = fixture(|_| {});
    // Charter marks SCRIBE crashed (daemon-restart marking or failed
    // restarts) — delivery is refused until supervision recovers it.
    let mut charter = Charter::load(&fx.tree.charter()).unwrap();
    charter.sessions.insert(
        SCRIBE,
        hamlet_core::session::SessionRecord {
            id: SessionId::new(SCRIBE, 0).unwrap(),
            status: SessionStatus::Crashed,
            restart_count: 3,
        },
    );
    charter.save_atomic(&fx.tree.charter()).unwrap();

    let text = mail_text("1002", SCRIBE, "Status?", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);
    let outcome = fx.router.route(&name("1001"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { reason, to } => {
            assert!(
                reason.to_string().contains("unavailable (state: crashed)"),
                "{reason}"
            );
            assert_eq!(to, Some(HERALD));
        }
        other => panic!("expected bounce, got {other:?}"),
    }
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 0);
    let bounce = route_bounce(&fx, HERALD);
    assert_eq!(bounce.header.subject, "Bounce: Status?");
}

#[tokio::test]
async fn bounce_queue_overflow() {
    let fx = fixture(|c| c.sessions.queue_depth = 2);
    for seq in [1u64, 2] {
        let text = mail_text("1002", SCRIBE, &format!("m{seq}"), "Body.");
        let file = write_spool(&fx.tree, SCRIBE, seq, HERALD, &text);
        assert!(matches!(
            fx.router.route(&name("1001"), &file).unwrap(),
            RouteOutcome::Delivered { .. }
        ));
    }
    let text = mail_text("1002", SCRIBE, "third", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 3, HERALD, &text);
    let outcome = fx.router.route(&name("1001"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { reason, to } => {
            assert_eq!(
                reason.to_string(),
                "delivery queue for UID 1001 full (depth 2)"
            );
            assert_eq!(to, Some(HERALD));
        }
        other => panic!("expected overflow bounce, got {other:?}"),
    }
    // Original consumed despite refusal; queue untouched at the bound.
    assert!(spool_names(&fx.tree, SCRIBE).is_empty());
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 2);
    // The bounce itself is deliverable to the (non-full) sender queue.
    let bounce = route_bounce(&fx, HERALD);
    assert_eq!(bounce.header.subject, "Bounce: third");
}

#[tokio::test]
async fn malformed_bounces_to_spool_writer() {
    let fx = fixture(|_| {});
    // Garbage content, conforming name: the writer uid from the file name
    // is the only trustworthy bounce target.
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    fs::create_dir_all(&dir).unwrap();
    let file = name("000000000009-1002-ff.mail");
    fs::write(dir.join(file.as_str()), b"not mail at all").unwrap();

    let outcome = fx.router.route(&name("1001"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { reason, to } => {
            assert!(
                reason.to_string().starts_with("malformed mail:"),
                "{reason}"
            );
            assert_eq!(to, Some(HERALD));
        }
        other => panic!("expected bounce, got {other:?}"),
    }
    let bounce = route_bounce(&fx, HERALD);
    assert_eq!(bounce.header.subject, "Bounce: (unparseable)");
    assert!(bounce.body.contains("malformed mail"));
}

#[tokio::test]
async fn malformed_foreign_name_has_no_bounce_target() {
    let fx = fixture(|_| {});
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    fs::create_dir_all(&dir).unwrap();
    let file = name("mystery.mail");
    fs::write(dir.join(file.as_str()), b"???").unwrap();
    let outcome = fx.router.route(&name("1001"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { to, .. } => assert_eq!(to, None),
        other => panic!("expected bounce, got {other:?}"),
    }
    // Consumed + traced, nothing written anywhere.
    assert!(spool_names(&fx.tree, SCRIBE).is_empty());
    assert_eq!(read_names(&fx.tree, SCRIBE), vec!["mystery.mail"]);
}

#[tokio::test]
async fn to_header_mismatch_bounces() {
    let fx = fixture(|_| {});
    // Mail To: HERALD dropped into SCRIBE's spool — routing refuses to
    // guess; the sender gets a malformed bounce.
    let text = mail_text("1002", HERALD, "Wrong box", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);
    let outcome = fx.router.route(&name("1001"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { reason, to } => {
            assert!(
                reason.to_string().contains("does not match spool owner"),
                "{reason}"
            );
            assert_eq!(to, Some(HERALD));
        }
        other => panic!("expected bounce, got {other:?}"),
    }
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 0);
}

#[tokio::test]
async fn named_sender_bounce_dies_with_trace() {
    let fx = fixture(|_| {});
    // Patron mail to a slotless recipient: no routable bounce target —
    // consume + trace, and never an infinite bounce chain.
    fs::create_dir_all(fx.tree.courier_mail_recipient(&name("1005"))).unwrap();
    let text = mail_text("patron-anna", 1005, "Hello?", "Body.");
    let file = write_spool(&fx.tree, 1005, 1, SCRIBE, &text);
    let outcome = fx.router.route(&name("1005"), &file).unwrap();
    match outcome {
        RouteOutcome::Bounced { to, .. } => assert_eq!(to, None),
        other => panic!("expected bounce, got {other:?}"),
    }
    assert!(spool_names(&fx.tree, SCRIBE).is_empty());
    assert!(spool_names(&fx.tree, HERALD).is_empty());
}

// ---------------------------------------------------------------------------
// Sending API (daemon-asserted identity)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_writes_atomic_spool_with_asserted_identity() {
    let fx = fixture(|_| {});
    fs::create_dir_all(fx.tree.courier_mail_recipient(&name("1001"))).unwrap();

    let path = fx
        .router
        .send(
            MailActor::Agent(HERALD),
            SCRIBE,
            "Findings",
            "See freight.",
            None,
        )
        .unwrap();
    // Conforming grammar: seq from the durable counter, writer = actor uid.
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let spool_ref = parse_spool_file_name(&file).expect("conforming name");
    assert_eq!(spool_ref.writer, HERALD);
    assert!(spool_ref.seq >= 1);
    let parsed = Mail::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed.header.from, MailSender::Uid(HERALD));
    assert_eq!(parsed.header.to, SCRIBE);
    // No temp litter; hidden temps never visible as incoming.
    let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Mayor + system + patron identities come from the actor, always.
    let mayor_path = fx
        .router
        .send(
            MailActor::Mayor,
            SCRIBE,
            "Decree soon",
            "Prepare.",
            Some("unix-42"),
        )
        .unwrap();
    let mayor = Mail::parse(&fs::read_to_string(&mayor_path).unwrap()).unwrap();
    assert_eq!(mayor.header.from, MailSender::Uid(1000));
    assert_eq!(mayor.header.date, "unix-42");

    let sys_path = fx
        .router
        .send(
            MailActor::System,
            SCRIBE,
            "Notice",
            "Daemon restarted.",
            None,
        )
        .unwrap();
    let sys_file = sys_path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(parse_spool_file_name(&sys_file).is_none(), "foreign name");
    assert!(sys_file.contains("-system-"), "{sys_file}");
    let sys = Mail::parse(&fs::read_to_string(&sys_path).unwrap()).unwrap();
    assert_eq!(sys.header.from, MailSender::Name(name("system")));

    let patron_path = fx
        .router
        .send(
            MailActor::Patron(name("anna")),
            SCRIBE,
            "Question",
            "How goes?",
            None,
        )
        .unwrap();
    let patron = Mail::parse(&fs::read_to_string(&patron_path).unwrap()).unwrap();
    assert_eq!(patron.header.from, MailSender::Name(name("anna")));

    // Sending never delivers directly: everything sits in the spool until
    // the (single) routing path consumes it.
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 0);
    let report = fx.router.route_recipient(&name("1001")).unwrap();
    assert_eq!(report.delivered, 4);
}

#[tokio::test]
async fn send_rejects_invalid_outgoing_mail() {
    let fx = fixture(|_| {});
    // Empty subject fails the format the daemon itself enforces.
    assert!(fx
        .router
        .send(MailActor::System, SCRIBE, "", "Body.", None)
        .is_err());
    // Empty body fails too.
    assert!(fx
        .router
        .send(MailActor::System, SCRIBE, "S", "", None)
        .is_err());
    // Non-slot recipient fails validation before any disk write.
    assert!(fx
        .router
        .send(MailActor::System, 3000, "S", "Body.", None)
        .is_err());
    assert!(spool_names(&fx.tree, SCRIBE).is_empty());
}

// ---------------------------------------------------------------------------
// Restart recovery (no duplicates — explicit CONVENTIONS §3 r6 test)
// ---------------------------------------------------------------------------

#[test]
fn restart_mid_delivery_recovers_from_ack_state_without_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    charter
        .allocate_slot(name("scribe"), Some(COMMISSION))
        .unwrap();
    charter
        .allocate_slot(name("herald"), Some(COMMISSION))
        .unwrap();
    charter.save_atomic(&tree.charter()).unwrap();

    let config = Config::default();
    let data_dir = tmp.path().join(".hamlet-data");
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));

    // Two mails pending; the daemon routes the first, then "dies" between
    // deliveries (mail B still in the spool, mail A acked + enqueued).
    let text_a = mail_text("1002", SCRIBE, "A", "first");
    let text_b = mail_text("1002", SCRIBE, "B", "second");
    let file_a = write_spool(&tree, SCRIBE, 1, HERALD, &text_a);
    let _file_b = write_spool(&tree, SCRIBE, 2, HERALD, &text_b);
    {
        let telemetry = Arc::new(Telemetry::open(&data_dir).unwrap());
        let supervisor = Arc::new(Supervisor::new(
            tree.clone(),
            config.clone(),
            Arc::clone(&spawner),
            Arc::clone(&telemetry),
        ));
        let router = MailRouter::new(Arc::clone(&supervisor));
        let outcome = router.route(&name("1001"), &file_a).unwrap();
        assert!(matches!(outcome, RouteOutcome::Delivered { .. }));
        // Daemon killed here: A persisted + acked, B untouched in spool.
    }

    // Restart: fresh supervisor (fresh QueueStore), fresh telemetry over
    // the same sled dir, recovery scan as the startup hook.
    let telemetry = Arc::new(Telemetry::open(&data_dir).unwrap());
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let router = MailRouter::new(Arc::clone(&supervisor));
    let report = router.recover_spools().unwrap();
    assert_eq!(
        (report.scanned, report.delivered, report.bounced),
        (1, 1, 0)
    );

    // Exactly two envelopes: A once (recovered from the queue file, NOT
    // re-routed — .read/ is never rescanned), B once (from the spool).
    let canonical = |t: &str| Mail::parse(t).unwrap().render();
    let pending = pending_texts(&supervisor, SCRIBE);
    assert_eq!(
        pending,
        vec![canonical(&text_a), canonical(&text_b)],
        "no duplicates, no drops"
    );
    let read = read_names(&tree, SCRIBE);
    assert_eq!(read, vec![file_a.as_str(), "000000000002-1002-2.mail"]);
    assert!(spool_names(&tree, SCRIBE).is_empty());
}

// ---------------------------------------------------------------------------
// Full plane: watcher + dispatcher + handler (events + recovery)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plane_end_to_end_recovery_then_events() {
    let fx = fixture(|_| {});
    // Seeded before startup → the recovery scan must deliver it.
    let seeded = mail_text("1002", SCRIBE, "seeded", "old body");
    write_spool(&fx.tree, SCRIBE, 1, HERALD, &seeded);

    let dispatcher = Dispatcher::new();
    let (_router, mut handle) =
        spawn_mail_plane(Arc::clone(&fx.supervisor), Arc::clone(&dispatcher)).unwrap();

    assert!(
        wait_until(
            || pending_texts(&fx.supervisor, SCRIBE).len() == 1,
            Duration::from_secs(10)
        )
        .await,
        "recovery delivers the seeded mail"
    );

    // A live spool write (atomic, as real writers do) routes via events.
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    let tmp = dir.join(".tmp-live.mail");
    fs::write(&tmp, mail_text("1002", SCRIBE, "live", "new body")).unwrap();
    fs::rename(&tmp, dir.join("000000000002-1002-ab.mail")).unwrap();

    assert!(
        wait_until(
            || pending_texts(&fx.supervisor, SCRIBE)
                .iter()
                .any(|t| t.contains("Subject: live")),
            Duration::from_secs(15)
        )
        .await,
        "event-driven route delivers the live mail"
    );
    assert_eq!(pending_texts(&fx.supervisor, SCRIBE).len(), 2);
    assert!(spool_names(&fx.tree, SCRIBE).is_empty(), "acked");
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Storm: 1k rapid spool writes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storm_1k_spool_writes_all_delivered_fifo() {
    const RECIPIENTS: u32 = 10; // 1003..=1012
    const PER_RECIPIENT: u64 = 100; // × 10 recipients = 1000 writes
    let fx = fixture(|c| c.sessions.queue_depth = 200);

    // Provision the storm recipients (slots + spool dirs created *before*
    // the watcher starts, so every dir is under an inotify watch).
    {
        let mut charter = Charter::load(&fx.tree.charter()).unwrap();
        for i in 0..RECIPIENTS {
            let uid = charter
                .allocate_slot(name(&format!("worker{i}")), Some(COMMISSION))
                .unwrap();
            assert_eq!(uid, SCRIBE + 2 + i);
            fs::create_dir_all(fx.tree.courier_mail_recipient(&name(&uid.to_string()))).unwrap();
        }
        charter.save_atomic(&fx.tree.charter()).unwrap();
    }

    let dispatcher = Dispatcher::new();
    let (_router, mut handle) =
        spawn_mail_plane(Arc::clone(&fx.supervisor), Arc::clone(&dispatcher)).unwrap();

    // 1000 rapid writes: hidden temp + rename per mail (the real writer
    // pattern), interleaved across recipients, seq ascending per writer.
    let mut by_recipient: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for seq in 0..PER_RECIPIENT {
        for r in 0..RECIPIENTS {
            let uid = SCRIBE + 2 + r;
            let text = mail_text(
                "1000",
                uid,
                &format!("msg-{seq:03}"),
                &format!("storm body {seq} for {uid}"),
            );
            let file = write_spool(&fx.tree, uid, seq, 1000, &text);
            by_recipient
                .entry(uid)
                .or_default()
                .push(file.into_string());
        }
    }

    // All 1000 delivered: every spool drained (acked) and every queue at
    // exactly PER_RECIPIENT depth — bounded channels apply backpressure,
    // nothing is dropped (lost events heal via hint-scans).
    let all_acked = || (0..RECIPIENTS).all(|r| spool_names(&fx.tree, SCRIBE + 2 + r).is_empty());
    let all_queued = || {
        (0..RECIPIENTS)
            .map(|r| fx.supervisor.queues().len(SCRIBE + 2 + r).unwrap_or(0))
            .sum::<usize>()
            == (u64::from(RECIPIENTS) * PER_RECIPIENT) as usize
    };
    assert!(
        wait_until(|| all_acked() && all_queued(), Duration::from_secs(180)).await,
        "storm fully delivered: acked={} queued={}",
        all_acked(),
        all_queued()
    );

    // Per-recipient FIFO: queue order == seq order == sorted spool-name
    // order, despite interleaved writers and event storms.
    for r in 0..RECIPIENTS {
        let uid = SCRIBE + 2 + r;
        let subjects: Vec<String> = pending_texts(&fx.supervisor, uid)
            .iter()
            .map(|t| t.lines().nth(2).unwrap().to_owned())
            .collect();
        let expected: Vec<String> = (0..PER_RECIPIENT)
            .map(|seq| format!("Subject: msg-{seq:03}"))
            .collect();
        assert_eq!(subjects, expected, "FIFO violated for uid {uid}");
        assert_eq!(read_names(&fx.tree, uid).len(), PER_RECIPIENT as usize);
    }
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Cross-checks against the tree contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queues_and_read_dirs_are_the_only_tree_mutations() {
    let fx = fixture(|_| {});
    let text = mail_text("1002", SCRIBE, "Audit", "Body.");
    let file = write_spool(&fx.tree, SCRIBE, 1, HERALD, &text);
    let before = snapshot_courier(&fx.tree);
    fx.router.route(&name("1001"), &file).unwrap();
    let after = snapshot_courier(&fx.tree);
    // Diff: spool file moved to .read/, queue file appeared — nothing else
    // (every daemon behavior observable from the tree, SPEC §1).
    let mut removed: Vec<_> = before.difference(&after).cloned().collect();
    let mut added: Vec<_> = after.difference(&before).cloned().collect();
    removed.sort();
    added.sort();
    assert_eq!(
        removed,
        vec!["mail/1001/000000000001-1002-1.mail".to_owned()]
    );
    assert_eq!(
        added,
        vec![
            "mail/1001/.read/000000000001-1002-1.mail".to_owned(),
            "queues/1001.jsonl".to_owned()
        ]
    );
    // Queue file is mode 0600 (mail text may be sensitive).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(fx.tree.session_queue(SCRIBE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

fn snapshot_courier(tree: &HamletTree) -> std::collections::BTreeSet<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(relative);
            }
        }
    }
    let mut out = Vec::new();
    walk(&tree.courier(), &tree.courier(), &mut out);
    out.into_iter().collect()
}
