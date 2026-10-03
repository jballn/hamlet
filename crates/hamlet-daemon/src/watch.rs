//! Filesystem watchers (SPEC §1 file-as-truth, §6, §8; notify per §3).
//!
//! inotify-backed watching of the whole `.hamlet/` tree drives mail delivery
//! (spool additions), DEED.md monitoring, decree detection (Mayor filesystem
//! emissions), dispatch outbox, and freight sandboxes. Event vocabulary,
//! classification and debounce policy are pure `hamlet_core::watch` types
//! (ADR-0019 cl. 5–6); this module runs `notify`, quiesces per file, and
//! dispatches typed [`Reaction`]s through the [`Dispatcher`] registry.
//! Events feed the governor loops; the tree + charter remain the only
//! state (CONVENTIONS §2 file-as-truth).
//!
//! # Pipeline (single dispatcher task)
//!
//! ```text
//! notify events ─▶ bounded raw channel ─▶ per-file quiesce (QUIESCE_MS,
//!   merge via hamlet_core::watch::merge_actions) ─▶ matured sweep (50 ms
//!   ticker) ─▶ EventBatch (sorted-path drain) ─▶ monotonic seq ─▶
//!   classify ─▶ Dispatcher fan-out ─▶ registered handler tasks
//! ```
//!
//! One task owns the whole chain, so reaction order is exactly
//! (sorted path, seq) — deterministic and reproducible (ADR-0019 cl. 6).
//! Hidden path components (writer temp files, `.read/` acks, queue temp
//! files) are dropped before debouncing; `classify` is the second guard.
//!
//! # Handler registration (extension point)
//!
//! [`Dispatcher::register`] hands a handler task a bounded receiver of
//! [`Dispatched`] items; handlers filter the [`Reaction`] variants they own
//! (mail now — DEED/decree/outbox/freight handlers register in later
//! sprints without touching this module). Fan-out is in sorted handler-name
//! order and `await`s each send, so a slow handler applies backpressure to
//! the watcher loop instead of growing memory without bound.
//!
//! # Startup hook & restart safety
//!
//! [`spawn_tree_watcher`] runs a `startup` closure **inside the watcher task
//! before any event is dispatched** — the mail plane uses it for spool
//! recovery ([`crate::mailroute::MailRouter::recover_spools`]), which
//! guarantees recovery and event-driven routing never race on the same
//! spool file. Event loss (kernel queue overflow, watcher downtime) is
//! healed by the same recovery on the next start: spool state is
//! tree-derivable (ADR-0019 consequences).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hamlet_core::tree::HamletTree;
use hamlet_core::watch::{
    classify, merge_actions, EventBatch, FsAction, FsEvent, Reaction, QUIESCE_MS,
};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Capacity of the bounded raw notify → debouncer channel. `notify`
/// delivers on its own thread and blocks when this is full, which pushes
/// back on the inotify kernel queue rather than growing the heap.
pub const RAW_CHANNEL_CAPACITY: usize = 8192;

/// Capacity of each bounded dispatcher → handler channel.
pub const HANDLER_CHANNEL_CAPACITY: usize = 4096;

/// Sweep interval of the debounce loop: matured per-file quiesce windows
/// are folded into the [`EventBatch`] at most this late (ADR-0019 cl. 6
/// leaves the sweep granularity to the daemon; ordering stays (path, seq)).
pub const SWEEP_MS: u64 = 50;

// ---------------------------------------------------------------------------
// Dispatcher — the handler registry (extension point for later sprints)
// ---------------------------------------------------------------------------

/// One classified event on its way to handlers: the monotonic drain
/// sequence number plus the typed reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    /// Watcher-lifetime monotonic sequence (assigned at batch drain).
    pub seq: u64,
    /// What the event means (never [`Reaction::Ignored`] — those are
    /// dropped at the watcher loop).
    pub reaction: Reaction,
}

/// Fan-out registry of named reaction handlers. Names sort the fan-out
/// order (deterministic, CONVENTIONS §3.1). Registering the same name
/// twice replaces the previous handler (its receiver then closes).
#[derive(Debug, Default)]
pub struct Dispatcher {
    handlers: Mutex<BTreeMap<&'static str, mpsc::Sender<Dispatched>>>,
}

impl Dispatcher {
    /// A new registry with a fresh dispatcher.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a handler under a static name and get its bounded reaction
    /// stream. The handler task owns the receiver; dropping it makes the
    /// dispatcher prune the entry on the next fan-out.
    pub fn register(&self, name: &'static str) -> mpsc::Receiver<Dispatched> {
        let (tx, rx) = mpsc::channel(HANDLER_CHANNEL_CAPACITY);
        self.handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name, tx);
        rx
    }

    /// Currently registered handler count (introspection for tests/CLI).
    pub fn handler_count(&self) -> usize {
        self.handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Fan one item out to every registered handler in sorted-name order.
    /// Awaits each send: a full handler channel pauses the watcher loop
    /// (bounded memory under event storms). Closed handlers are pruned.
    pub async fn dispatch(&self, item: Dispatched) {
        let senders: Vec<(&'static str, mpsc::Sender<Dispatched>)> = self
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, tx)| (*name, tx.clone()))
            .collect();
        let mut dead = Vec::new();
        for (name, tx) in &senders {
            if tx.send(item.clone()).await.is_err() {
                dead.push(*name);
            }
        }
        if !dead.is_empty() {
            let mut handlers = self.handlers.lock().unwrap_or_else(|e| e.into_inner());
            for name in dead {
                handlers.remove(name);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Watcher
// ---------------------------------------------------------------------------

/// Typed watcher startup failures.
#[derive(Debug)]
pub enum WatchError {
    /// `notify` backend failure.
    Notify(notify::Error),
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Notify(e) => write!(f, "notify watcher: {e}"),
        }
    }
}

impl std::error::Error for WatchError {}

impl From<notify::Error> for WatchError {
    fn from(e: notify::Error) -> Self {
        Self::Notify(e)
    }
}

/// Handle of a running watcher task. [`WatcherHandle::stop`] shuts it down
/// gracefully; dropping the handle signals shutdown best-effort (the task
/// then ends and the `notify` watcher is released).
#[must_use = "dropping the handle signals the watcher to stop"]
pub struct WatcherHandle {
    shutdown: mpsc::Sender<()>,
    task: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Stop the watcher and wait for the task to finish. Pending
    /// (un-matured) debounce entries are discarded — recovery-on-start
    /// re-derives any missed work from the tree (file-as-truth).
    pub async fn stop(&mut self) {
        let _ = self.shutdown.send(()).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Watch the whole `.hamlet/` tree and dispatch typed reactions.
///
/// `startup` runs inside the watcher task **before** the event loop, so
/// recovery scans ordered before the first dispatch can never race with
/// event-driven handlers. Must be called from within a tokio runtime.
///
/// # Errors
///
/// [`WatchError::Notify`] when the backend watcher cannot be created or
/// cannot watch the tree root.
pub fn spawn_tree_watcher(
    tree: HamletTree,
    dispatcher: Arc<Dispatcher>,
    startup: impl FnOnce() + Send + 'static,
) -> Result<WatcherHandle, WatchError> {
    let (raw_tx, raw_rx) = mpsc::channel::<Event>(RAW_CHANNEL_CAPACITY);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        if let Ok(event) = result {
            // Runs on the notify thread (never a tokio worker): blocking
            // send is the bounded-memory bridge into the async loop.
            let _ = raw_tx.blocking_send(event);
        }
    })?;
    watcher.watch(tree.root(), RecursiveMode::Recursive)?;
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
    let task = tokio::spawn(watch_loop(
        tree,
        dispatcher,
        watcher,
        raw_rx,
        shutdown_rx,
        startup,
    ));
    Ok(WatcherHandle {
        shutdown: shutdown_tx,
        task: Some(task),
    })
}

/// One debouncing entry: the folded event plus its quiesce deadline.
struct Pending {
    event: FsEvent,
    deadline: Instant,
}

async fn watch_loop(
    tree: HamletTree,
    dispatcher: Arc<Dispatcher>,
    _watcher: RecommendedWatcher, // kept alive for the loop's lifetime
    mut raw_rx: mpsc::Receiver<Event>,
    mut shutdown_rx: mpsc::Receiver<()>,
    startup: impl FnOnce(),
) {
    startup();
    let mut pending: BTreeMap<PathBuf, Pending> = BTreeMap::new();
    let mut batch = EventBatch::new();
    let mut seq: u64 = 0;
    let mut ticker = tokio::time::interval(Duration::from_millis(SWEEP_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            maybe_event = raw_rx.recv() => {
                let Some(event) = maybe_event else { break };
                ingest(&tree, event, &mut pending);
            }
            _ = ticker.tick() => {
                sweep(&mut pending, &mut batch);
                for event in batch.drain() {
                    seq += 1;
                    let reaction = classify(&event);
                    if !matches!(reaction, Reaction::Ignored) {
                        dispatcher.dispatch(Dispatched { seq, reaction }).await;
                    }
                }
            }
        }
    }
}

/// Normalize one notify event into village-relative [`FsEvent`]s, drop
/// hidden components, and fold them into the per-file quiesce buffer.
fn ingest(tree: &HamletTree, event: Event, pending: &mut BTreeMap<PathBuf, Pending>) {
    let Some(action) = map_action(&event.kind) else {
        return;
    };
    for path in event.paths {
        let Some(relative) = relative_to_root(tree.root(), &path) else {
            continue;
        };
        if has_hidden_component(&relative) {
            continue;
        }
        // Directory-ness is best effort at event time: only Created+dir
        // reactions (freight sandboxes) read it, and removals are Ignored.
        let is_dir = std::fs::metadata(&path).is_ok_and(|m| m.is_dir());
        let Some(folded) = FsEvent::new(action, relative.clone(), is_dir) else {
            continue;
        };
        let deadline = Instant::now() + Duration::from_millis(QUIESCE_MS);
        match pending.get_mut(&relative) {
            Some(entry) => match merge_actions(entry.event.action, action) {
                Some(merged) => {
                    entry.event = FsEvent {
                        action: merged,
                        path: relative,
                        is_dir,
                    };
                    entry.deadline = deadline;
                }
                None => {
                    pending.remove(&relative);
                }
            },
            None => {
                pending.insert(
                    relative,
                    Pending {
                        event: folded,
                        deadline,
                    },
                );
            }
        }
    }
}

/// Move quiesced entries into the batch (sorted-path drain order).
fn sweep(pending: &mut BTreeMap<PathBuf, Pending>, batch: &mut EventBatch) {
    let now = Instant::now();
    let matured: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, entry)| entry.deadline <= now)
        .map(|(path, _)| path.clone())
        .collect();
    for path in matured {
        if let Some(entry) = pending.remove(&path) {
            batch.insert(entry.event);
        }
    }
}

/// Map a notify [`EventKind`] onto the ADR-0019 action vocabulary;
/// access/other noise is dropped at the boundary.
fn map_action(kind: &EventKind) -> Option<FsAction> {
    match kind {
        EventKind::Create(_) => Some(FsAction::Created),
        EventKind::Remove(_) => Some(FsAction::Removed),
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_)) => {
            Some(FsAction::Modified)
        }
        // Rename targets count as Created, sources as Removed (ADR-0019
        // cl. 5 docs); inotify reports paired From/To name events.
        EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both)) => {
            Some(FsAction::Created)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => Some(FsAction::Removed),
        EventKind::Modify(
            ModifyKind::Name(RenameMode::Any) | ModifyKind::Any | ModifyKind::Other,
        ) => Some(FsAction::Modified),
        _ => None,
    }
}

/// Village-root-relative form of an absolute notify path; `None` when the
/// path is outside the watched root or is the root itself.
fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).ok()?;
    (!relative.as_os_str().is_empty()).then(|| relative.to_path_buf())
}

/// True when any component is dot-prefixed (writer temp files, `.read/`
/// acks, daemon internals — ADR-0019 cl. 2–3 invisibility rule).
fn has_hidden_component(relative: &Path) -> bool {
    relative.components().any(|component| {
        matches!(component, Component::Normal(seg) if seg.to_string_lossy().starts_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_mapping_covers_rename_semantics() {
        use notify::event::{CreateKind, DataChange, RemoveKind};
        assert_eq!(
            map_action(&EventKind::Create(CreateKind::File)),
            Some(FsAction::Created)
        );
        assert_eq!(
            map_action(&EventKind::Modify(ModifyKind::Name(RenameMode::To))),
            Some(FsAction::Created)
        );
        assert_eq!(
            map_action(&EventKind::Modify(ModifyKind::Name(RenameMode::From))),
            Some(FsAction::Removed)
        );
        assert_eq!(
            map_action(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            Some(FsAction::Modified)
        );
        assert_eq!(
            map_action(&EventKind::Remove(RemoveKind::File)),
            Some(FsAction::Removed)
        );
        assert_eq!(
            map_action(&EventKind::Access(notify::event::AccessKind::Read)),
            None
        );
        assert_eq!(map_action(&EventKind::Other), None);
    }

    #[test]
    fn hidden_components_are_filtered_before_debounce() {
        assert!(has_hidden_component(Path::new(
            "courier/mail/1001/.read/x.mail"
        )));
        assert!(has_hidden_component(Path::new(
            "courier/mail/1001/.tmp-7.mail"
        )));
        assert!(has_hidden_component(Path::new(".hamlet-data/x")));
        assert!(!has_hidden_component(Path::new("courier/mail/1001/x.mail")));
    }

    #[test]
    fn relative_to_root_rejects_foreign_and_root_paths() {
        let root = Path::new("/village/.hamlet");
        assert_eq!(
            relative_to_root(root, Path::new("/village/.hamlet/courier/mail")),
            Some(PathBuf::from("courier/mail"))
        );
        assert_eq!(relative_to_root(root, Path::new("/village/.hamlet")), None);
        assert_eq!(relative_to_root(root, Path::new("/elsewhere/x")), None);
    }
}
