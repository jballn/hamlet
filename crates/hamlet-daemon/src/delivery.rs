//! Persistent per-session delivery queues (SPEC §6 sequential delivery;
//! Sprint 03 step 03).
//!
//! One bounded FIFO of [`SessionEnvelope`]s per UID slot. The queue is
//! **file-as-truth** (CONVENTIONS §2): every mutation is written through to
//! `courier/queues/<uid>.jsonl` (typed accessor [`HamletTree::session_queue`],
//! ADR-0003 — no ad-hoc paths) as one canonical envelope JSONL record per
//! line, with charter-grade atomic discipline (unique temp in the same
//! directory → write → fsync → mode 0600 → rename → fsync dir; ADR-0006).
//! An empty queue removes its file — the tree never carries litter.
//!
//! The in-memory `BTreeMap<u32, VecDeque<…>>` is a *cache* of that file,
//! loaded lazily on first touch per uid. The daemon is the singular
//! governor and this store its only writer, so the cache cannot diverge;
//! a daemon restart gets a fresh store that recovers the undelivered queue
//! from the tree (CONVENTIONS §3 r6 restart-resumability).
//!
//! # Delivery contract (consumed by [`crate::supervise`])
//!
//! - **At-most-once**: [`QueueStore::pop`] removes-and-persists *before*
//!   the envelope is written to the session's stdin — a crash between pop
//!   and write loses that one envelope (traced by the supervisor) rather
//!   than risk a duplicate delivery after restart. Undelivered items
//!   survive; half-delivered ones do not replay.
//! - **Backpressure**: [`QueueStore::enqueue`] enforces the configured
//!   bound (`sessions.queue_depth`) and answers overflow with the typed
//!   [`QueueError::Overflow`] — the bounce hook Sprint 04's mail plumbing
//!   routes back to the sender.
//! - **Corruption is a hard typed error** ([`QueueError::Corrupt`] with
//!   the offending line number, never the line content — mail text may be
//!   sensitive, CONVENTIONS §6); nothing is silently repaired or dropped.
//!
//! All methods are synchronous and hold the store mutex only across pure
//! cache work + file I/O — never across an await point.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use hamlet_core::session::{SessionEnvelope, SessionError};
use hamlet_core::tree::HamletTree;

/// Typed delivery-queue failures.
#[derive(Debug)]
pub enum QueueError {
    /// The queue is at its configured bound — the envelope was NOT enqueued.
    /// This is the bounce hook: Sprint 04's mail router turns it into a
    /// bounce back to the sender (SPEC §8.1).
    Overflow {
        /// Recipient slot.
        uid: u32,
        /// The bound in force (`sessions.queue_depth`).
        depth: usize,
    },
    /// The persisted queue file failed to parse. Carries the 1-based line
    /// number and the parser's reason — never the line content (it may
    /// carry mail text; CONVENTIONS §6).
    Corrupt {
        /// Queue owner.
        uid: u32,
        /// 1-based line number in `<uid>.jsonl`.
        line: usize,
        /// Parser diagnostic.
        reason: String,
    },
    /// Envelope construction/validation failure.
    Session(SessionError),
    /// Filesystem failure with context.
    Io {
        /// What was attempted.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow { uid, depth } => {
                write!(f, "delivery queue for uid {uid} is full (depth {depth})")
            }
            Self::Corrupt { uid, line, reason } => {
                write!(
                    f,
                    "delivery queue for uid {uid} is corrupt at line {line}: {reason}"
                )
            }
            Self::Session(e) => write!(f, "session: {e}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for QueueError {}

impl From<SessionError> for QueueError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}

/// The per-village delivery-queue store: file-backed FIFOs keyed by uid,
/// with a lazily-loaded in-memory cache (see module docs).
pub struct QueueStore {
    tree: HamletTree,
    cache: Mutex<BTreeMap<u32, VecDeque<SessionEnvelope>>>,
    tmp_serial: AtomicU64,
}

impl QueueStore {
    /// Store over a village tree. Pure — no I/O until first use.
    #[must_use]
    pub fn new(tree: HamletTree) -> Self {
        Self {
            tree,
            cache: Mutex::new(BTreeMap::new()),
            tmp_serial: AtomicU64::new(0),
        }
    }

    /// Enqueue one envelope at the back, persisting immediately. Returns
    /// the new queue depth.
    ///
    /// # Errors
    ///
    /// [`QueueError::Overflow`] when the queue already holds `max_depth`
    /// envelopes (the envelope is not enqueued — bounce hook), persistence
    /// failures, or a corrupt existing queue file.
    pub fn enqueue(
        &self,
        uid: u32,
        envelope: SessionEnvelope,
        max_depth: usize,
    ) -> Result<usize, QueueError> {
        self.write_with(uid, |queue| {
            if queue.len() >= max_depth {
                return Err(QueueError::Overflow {
                    uid,
                    depth: max_depth,
                });
            }
            queue.push_back(envelope);
            Ok(queue.len())
        })
    }

    /// Remove and return the front envelope (FIFO), persisting the removal
    /// **before** the caller writes it to the session (at-most-once).
    ///
    /// # Errors
    ///
    /// Persistence failures or a corrupt queue file.
    pub fn pop(&self, uid: u32) -> Result<Option<SessionEnvelope>, QueueError> {
        self.write_with(uid, |queue| Ok(queue.pop_front()))
    }

    /// Current undelivered depth (loads from the tree on first touch).
    ///
    /// # Errors
    ///
    /// A corrupt queue file.
    pub fn len(&self, uid: u32) -> Result<usize, QueueError> {
        self.read_with(uid, |queue| Ok(queue.len()))
    }

    /// Is the queue empty?
    ///
    /// # Errors
    ///
    /// A corrupt queue file.
    pub fn is_empty(&self, uid: u32) -> Result<bool, QueueError> {
        self.read_with(uid, |queue| Ok(queue.is_empty()))
    }

    /// Snapshot of the undelivered envelopes, front first (introspection
    /// for tests, the CLI, and Sprint 04 bounce rendering).
    ///
    /// # Errors
    ///
    /// A corrupt queue file.
    pub fn pending(&self, uid: u32) -> Result<Vec<SessionEnvelope>, QueueError> {
        self.read_with(uid, |queue| Ok(queue.iter().cloned().collect()))
    }

    fn read_with<T>(
        &self,
        uid: u32,
        f: impl FnOnce(&VecDeque<SessionEnvelope>) -> Result<T, QueueError>,
    ) -> Result<T, QueueError> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let queue = self.cached(&mut cache, uid)?;
        f(queue)
    }

    fn write_with<T>(
        &self,
        uid: u32,
        f: impl FnOnce(&mut VecDeque<SessionEnvelope>) -> Result<T, QueueError>,
    ) -> Result<T, QueueError> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let queue = self.cached(&mut cache, uid)?;
        let out = f(queue)?;
        // Persist after a successful mutation only — a refused enqueue
        // (overflow) must not rewrite the file.
        self.flush(uid, queue)?;
        Ok(out)
    }

    fn cached<'a>(
        &self,
        cache: &'a mut BTreeMap<u32, VecDeque<SessionEnvelope>>,
        uid: u32,
    ) -> Result<&'a mut VecDeque<SessionEnvelope>, QueueError> {
        if let std::collections::btree_map::Entry::Vacant(entry) = cache.entry(uid) {
            entry.insert(self.load_file(uid)?);
        }
        Ok(cache
            .get_mut(&uid)
            .expect("entry inserted or fetched above"))
    }

    fn load_file(&self, uid: u32) -> Result<VecDeque<SessionEnvelope>, QueueError> {
        let path = self.tree.session_queue(uid);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(VecDeque::new()),
            Err(e) => {
                return Err(QueueError::Io {
                    context: format!("read {}", path.display()),
                    source: e,
                })
            }
        };
        let mut out = VecDeque::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match SessionEnvelope::from_json_line(line) {
                Ok(envelope) => out.push_back(envelope),
                Err(e) => {
                    return Err(QueueError::Corrupt {
                        uid,
                        line: index + 1,
                        reason: e.to_string(),
                    })
                }
            }
        }
        Ok(out)
    }

    /// Atomic rewrite of the whole queue file (ADR-0006 discipline); an
    /// empty queue removes the file.
    fn flush(&self, uid: u32, queue: &VecDeque<SessionEnvelope>) -> Result<(), QueueError> {
        let path = self.tree.session_queue(uid);
        let dir = self.tree.courier_queues();
        if queue.is_empty() {
            return match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(QueueError::Io {
                    context: format!("remove {}", path.display()),
                    source: e,
                }),
            };
        }
        fs::create_dir_all(&dir).map_err(|e| QueueError::Io {
            context: format!("create {}", dir.display()),
            source: e,
        })?;
        let serial = self.tmp_serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".queue-{uid}.{}.{serial}.tmp", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            for envelope in queue {
                writeln!(file, "{}", envelope.to_json_line())?;
            }
            file.sync_all()?;
            set_mode_0600(&tmp)?; // mail text may be sensitive (CONVENTIONS §6)
            fs::rename(&tmp, &path)?;
            if let Ok(dir_handle) = fs::File::open(&dir) {
                let _ = dir_handle.sync_all();
            }
            Ok(())
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&tmp);
            return Err(QueueError::Io {
                context: format!("write {}", path.display()),
                source,
            });
        }
        Ok(())
    }
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hamlet_core::session::SessionId;

    fn store() -> (tempfile::TempDir, QueueStore) {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        let store = QueueStore::new(tree);
        (tmp, store)
    }

    fn session() -> SessionId {
        SessionId::new(1001, 0).unwrap()
    }

    fn mail(seq: u64, text: &str) -> SessionEnvelope {
        SessionEnvelope::mail(&session(), seq, text).unwrap()
    }

    #[test]
    fn fifo_order_and_depth() {
        let (_tmp, store) = store();
        for (seq, text) in [(1, "first"), (2, "second"), (3, "third")] {
            let depth = store.enqueue(1001, mail(seq, text), 8).unwrap();
            assert_eq!(depth, seq as usize);
        }
        assert_eq!(store.len(1001).unwrap(), 3);
        assert!(!store.is_empty(1001).unwrap());
        let texts: Vec<String> = (0..3)
            .map(|_| match store.pop(1001).unwrap().unwrap() {
                SessionEnvelope::Mail { text, .. } => text,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(texts, ["first", "second", "third"]);
        assert!(store.pop(1001).unwrap().is_none());
        assert!(store.is_empty(1001).unwrap());
    }

    #[test]
    fn overflow_is_typed_and_does_not_enqueue() {
        let (_tmp, store) = store();
        store.enqueue(1001, mail(1, "a"), 2).unwrap();
        store.enqueue(1001, mail(2, "b"), 2).unwrap();
        let err = store.enqueue(1001, mail(3, "c"), 2).unwrap_err();
        assert!(
            matches!(
                err,
                QueueError::Overflow {
                    uid: 1001,
                    depth: 2
                }
            ),
            "{err}"
        );
        assert_eq!(store.len(1001).unwrap(), 2, "refused item stayed out");
    }

    /// The undelivered queue survives a store replacement (daemon restart,
    /// CONVENTIONS §3 r6) — recovered from the tree, same order.
    #[test]
    fn queue_survives_store_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        {
            let store = QueueStore::new(tree.clone());
            store.enqueue(1001, mail(1, "persisted-one"), 8).unwrap();
            store.enqueue(1001, mail(2, "persisted-two"), 8).unwrap();
        }
        // File-as-truth: canonical JSONL at the typed accessor, mode 0600.
        let path = tree.session_queue(1001);
        let raw = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""kind":"mail""#) && lines[0].contains("m-1-1001.0"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No temp litter.
        let litter: Vec<_> = fs::read_dir(tree.courier_queues())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(litter.is_empty(), "{litter:?}");

        let store = QueueStore::new(tree.clone());
        let pending = store.pending(1001).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(
            store.pop(1001).unwrap().unwrap().correlation().as_str(),
            "m-1-1001.0"
        );
        assert_eq!(
            store.pop(1001).unwrap().unwrap().correlation().as_str(),
            "m-2-1001.0"
        );
        // Draining to empty removes the file (no tree litter).
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_queue_is_typed_error_without_content_leak() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        fs::create_dir_all(tree.courier_queues()).unwrap();
        let good = mail(1, "secret-mail-text-do-not-log").to_json_line();
        fs::write(
            tree.session_queue(1001),
            format!("{good}\nnot-json-at-all {{{{\n"),
        )
        .unwrap();
        let store = QueueStore::new(tree);
        let err = store.len(1001).unwrap_err();
        match err {
            QueueError::Corrupt { uid, line, reason } => {
                assert_eq!(uid, 1001);
                assert_eq!(line, 2);
                assert!(!reason.contains("not-json-at-all"));
            }
            other => panic!("expected Corrupt, got {other}"),
        }
        // The rendered error must not carry queue content either.
        let rendered = store.pop(1001).unwrap_err().to_string();
        assert!(!rendered.contains("secret-mail-text"));
    }

    #[test]
    fn unknown_uid_is_empty_not_an_error() {
        let (_tmp, store) = store();
        assert_eq!(store.len(1002).unwrap(), 0);
        assert!(store.pop(1002).unwrap().is_none());
        assert!(store.pending(1002).unwrap().is_empty());
    }

    #[test]
    fn interrupt_envelopes_are_refused_from_the_queue_store_caller_not_the_store() {
        // The store itself is envelope-agnostic; the supervisor only ever
        // enqueues queued-kind envelopes (interrupts bypass the queue,
        // SPEC §6). Round-tripping an interrupt through persistence must
        // still be lossless, because recovered files are parsed as-is.
        let (_tmp, store) = store();
        let interrupt = SessionEnvelope::interrupt(
            &session(),
            9,
            hamlet_core::session::InterruptSource::Human,
            hamlet_core::session::InterruptPriority::Critical,
            "halt",
        )
        .unwrap();
        store.enqueue(1001, interrupt.clone(), 4).unwrap();
        assert_eq!(store.pop(1001).unwrap().unwrap(), interrupt);
    }
}
