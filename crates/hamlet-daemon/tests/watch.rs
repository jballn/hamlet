//! Tree watcher core integration tests (Sprint 04 step 02; SPEC §8,
//! ADR-0019 cl. 5–6 — CONVENTIONS §9 doctrine: tempdir tree, no podman).
//!
//! Real `notify`/inotify over a tempdir `.hamlet/` tree: spool additions
//! become `MailSpool` reactions, hidden/unmapped noise never dispatches,
//! create+remove blips cancel inside the quiesce window, dir reactions
//! (freight/outbox) carry their typed payloads, the startup hook runs
//! strictly before the first dispatch, and `stop` is clean.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::CorrelationId;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use hamlet_daemon::watch::{spawn_tree_watcher, Dispatched, Dispatcher, WatcherHandle};

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    _dispatcher: Arc<Dispatcher>,
    handle: WatcherHandle,
    rx: tokio::sync::mpsc::Receiver<Dispatched>,
}

/// Spawn the watcher over a fresh tempdir tree with one collector
/// handler registered.
async fn fixture(startup: impl FnOnce() + Send + 'static) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let dispatcher = Dispatcher::new();
    let rx = dispatcher.register("collector");
    let handle = spawn_tree_watcher(tree.clone(), Arc::clone(&dispatcher), startup).unwrap();
    // Give inotify a moment to attach recursive watches; spool dirs are
    // pre-created like `MailRouter::ensure_spool_dirs` does at startup
    // (events inside a *freshly created* dir can precede its watch).
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::create_dir_all(tree.courier_mail_recipient(&name("1001"))).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    Fixture {
        _tmp: tmp,
        tree,
        _dispatcher: dispatcher,
        handle,
        rx,
    }
}

impl Fixture {
    /// Atomic writer-visibility pattern (ADR-0019 cl. 2): hidden temp +
    /// rename into the spool.
    fn write_mail_atomic(&self, recipient: &str, file: &str, body: &str) -> PathBuf {
        let dir = self.tree.courier_mail_recipient(&name(recipient));
        fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(format!(".tmp-{file}"));
        fs::write(&tmp, body).unwrap();
        let final_path = dir.join(file);
        fs::rename(&tmp, &final_path).unwrap();
        final_path
    }

    async fn next(&mut self, within: Duration) -> Option<Dispatched> {
        tokio::time::timeout(within, self.rx.recv())
            .await
            .ok()
            .flatten()
    }

    async fn expect_none(&mut self, within: Duration) {
        assert!(
            self.next(within).await.is_none(),
            "no reaction may be dispatched in this window"
        );
    }
}

const WINDOW: Duration = Duration::from_secs(10);

#[tokio::test]
async fn spool_write_becomes_mailspool_reaction() {
    let mut fx = fixture(|| {}).await;
    fx.write_mail_atomic(
        "1001",
        "000000000001-1002-ab12.mail",
        "From: 1002\nTo: 1001\nSubject: Hi\nDate: unix-1\n\nBody.\n",
    );
    let item = fx.next(WINDOW).await.expect("reaction dispatched");
    assert_eq!(
        item.reaction,
        Reaction::MailSpool {
            recipient: name("1001"),
            file: name("000000000001-1002-ab12.mail"),
        }
    );
    assert!(item.seq >= 1, "dispatch carries a drain sequence number");
    fx.handle.stop().await;
}

#[tokio::test]
async fn hidden_and_unmapped_events_never_dispatch() {
    let mut fx = fixture(|| {}).await;
    // Writer temp files (hidden) — invisible.
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".tmp-scratch.mail"), b"noise").unwrap();
    // Non-mail file in a spool dir — Ignored.
    fs::write(dir.join("notes.txt"), b"noise").unwrap();
    // Charter mutation and queue-file rewrite — unmapped daemon internals.
    fs::write(fx.tree.charter(), b"{\"charter_version\": 2}").unwrap();
    fs::create_dir_all(fx.tree.courier_queues()).unwrap();
    fs::write(fx.tree.session_queue(1001), b"{}\n").unwrap();
    // Ack-dir activity — invisible.
    let read = fx.tree.courier_mail_read(&name("1001"));
    fs::create_dir_all(&read).unwrap();
    fs::write(read.join("old.mail"), b"acked").unwrap();

    fx.expect_none(Duration::from_millis(900)).await;

    // A real spool addition still dispatches — the noise above consumed
    // no handler attention.
    fx.write_mail_atomic("1001", "000000000002-1002-cd.mail", "x");
    let item = fx.next(WINDOW).await.expect("real mail dispatches");
    assert_eq!(
        item.reaction,
        Reaction::MailSpool {
            recipient: name("1001"),
            file: name("000000000002-1002-cd.mail"),
        }
    );
    fx.handle.stop().await;
}

#[tokio::test]
async fn create_remove_blip_cancels_inside_quiesce_window() {
    let mut fx = fixture(|| {}).await;
    let dir = fx.tree.courier_mail_recipient(&name("1001"));
    fs::create_dir_all(&dir).unwrap();
    let blip = dir.join("000000000003-1002-ee.mail");
    fs::write(&blip, b"blip").unwrap();
    // Remove well inside the 250 ms quiesce window → Created+Removed
    // cancels (ADR-0019 cl. 6).
    fs::remove_file(&blip).unwrap();
    fx.expect_none(Duration::from_millis(900)).await;
    fx.handle.stop().await;
}

#[tokio::test]
async fn dir_reactions_carry_typed_payloads() {
    let mut fx = fixture(|| {}).await;
    // Freight sandbox dir creation.
    fs::create_dir_all(fx.tree.courier_freight()).unwrap();
    fs::create_dir(fx.tree.courier_freight().join("report-7")).unwrap();
    // Dispatch outbox addition.
    fs::write(
        fx.tree.courier_dispatch_outbox().join("d-1-1000.1.json"),
        b"{}",
    )
    .unwrap();

    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.push(fx.next(WINDOW).await.expect("two reactions"));
    }
    assert!(seen.iter().any(|i| i.reaction
        == Reaction::FreightDir {
            name: name("report-7")
        }));
    assert!(seen.iter().any(|i| i.reaction
        == Reaction::DispatchOutbox {
            correlation: CorrelationId::parse("d-1-1000.1").unwrap()
        }));
    // Drain order is (path, seq): whichever cycle they mature in, the
    // dispatch sequence numbers strictly increase.
    assert!(seen[0].seq < seen[1].seq, "monotonic drain sequence");
    fx.handle.stop().await;
}

#[tokio::test]
async fn startup_hook_runs_before_first_dispatch() {
    let flag = Arc::new(AtomicBool::new(false));
    let startup_flag = Arc::clone(&flag);
    let mut fx = fixture(move || {
        // Simulated recovery work: strictly ordered before event dispatch.
        std::thread::sleep(Duration::from_millis(50));
        startup_flag.store(true, Ordering::SeqCst);
    })
    .await;
    fx.write_mail_atomic("1001", "000000000004-1002-aa.mail", "x");
    let _ = fx.next(WINDOW).await.expect("reaction after startup");
    // The startup hook runs inside the watcher task strictly before the
    // event loop, so by the time any reaction is dispatched it has run.
    assert!(flag.load(Ordering::SeqCst), "startup ran before dispatch");
    fx.handle.stop().await;
}

#[tokio::test]
async fn dispatcher_fans_out_to_every_registered_handler() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let dispatcher = Dispatcher::new();
    let mut rx_a = dispatcher.register("a");
    let mut rx_b = dispatcher.register("b");
    assert_eq!(dispatcher.handler_count(), 2);
    let mut handle = spawn_tree_watcher(tree.clone(), Arc::clone(&dispatcher), || {}).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let dir = tree.courier_mail_recipient(&name("1001"));
    fs::create_dir_all(&dir).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(dir.join("000000000005-1002-bb.mail"), b"x").unwrap();
    let a = tokio::time::timeout(WINDOW, rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(WINDOW, rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a, b, "same item reaches every handler");
    // A dropped handler is pruned on the next fan-out; the survivor keeps
    // receiving (resilient registration).
    drop(rx_b);
    fs::write(dir.join("000000000006-1002-cc.mail"), b"x").unwrap();
    let _ = tokio::time::timeout(WINDOW, rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    fs::write(dir.join("000000000007-1002-dd.mail"), b"x").unwrap();
    let _ = tokio::time::timeout(WINDOW, rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dispatcher.handler_count(), 1);
    handle.stop().await;
}

#[tokio::test]
async fn stop_terminates_the_watcher_task() {
    let mut fx = fixture(|| {}).await;
    // stop() must return (not hang) even with pending un-matured events.
    fx.write_mail_atomic("1001", "000000000008-1002-ef.mail", "x");
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.next(Duration::from_millis(100)).await.is_none() {
        assert!(Instant::now() < deadline, "reaction never arrived");
    }
    tokio::time::timeout(Duration::from_secs(10), fx.handle.stop())
        .await
        .expect("stop must not hang");
}
