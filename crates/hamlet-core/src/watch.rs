//! Watcher event vocabulary: typed FsEvent → Reaction mapping (SPEC §8;
//! ADR-0019 cl. 5–6).
//!
//! The pure half of the file-as-truth event plane. `hamlet-daemon::watch`
//! runs the `notify` watcher, debounces, and calls [`classify`] — this
//! module owns *what an event means*, never the watching itself (purity
//! seam, CONVENTIONS §2).
//!
//! # Mapping table (ADR-0019 cl. 5)
//!
//! | Region (village-relative path) | Action | Reaction | Consumer |
//! |---|---|---|---|
//! | `courier/mail/<r>/<f>.mail` (file) | Created | [`Reaction::MailSpool`] | daemon `mailroute` (Sprint 04) |
//! | `keep/census/parcel<uid>/DEED.md` | Created/Modified | [`Reaction::DeedMutation`] | Sprint 06 |
//! | `keep/census/manor/home/**` (file) | Created/Modified | [`Reaction::DecreeWrite`] | Sprint 05 |
//! | `guild/ledger/commission<n>/INDENTURE.md` | Created/Modified | [`Reaction::DecreeWrite`] | Sprint 05 |
//! | `courier/dispatch/outbox/<f>.json` | Created | [`Reaction::DispatchOutbox`] | daemon `dispatch` (Sprint 04) |
//! | `courier/freight/<name>/` (dir) | Created | [`Reaction::FreightDir`] | daemon `freight` (Sprint 04) |
//! | anything else / hidden components | any | [`Reaction::Ignored`] | — |
//!
//! Hidden (dot-prefixed) components anywhere in the path make an event
//! `Ignored` — that is the writer temp-file and `.read/` ack invisibility
//! rule (ADR-0019 cl. 2–3). Removals are `Ignored` everywhere: consumption
//! is by rename, and a removal never carries work.
//!
//! # Debounce & ordering (ADR-0019 cl. 6)
//!
//! Per-file quiesce window [`QUIESCE_MS`]; [`EventBatch`] merges events per
//! path and drains in sorted-path order — deterministic and reproducible.
//! The daemon stamps monotonic sequence numbers at drain time.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::sanitize::{sanitize_name, SafeName};
use crate::session::CorrelationId;

/// Per-file quiesce window in ms: an event is dispatched only after no
/// further event for the same path for this long (ADR-0019 cl. 6).
pub const QUIESCE_MS: u64 = 250;

/// The filesystem actions the daemon maps `notify` events onto. Everything
/// else notify reports (access, scan steps) is dropped at the watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FsAction {
    /// File/dir appeared (rename target counts as Created).
    Created,
    /// Existing file/dir content changed.
    Modified,
    /// File/dir disappeared (rename source counts as Removed).
    Removed,
}

/// One debounced filesystem event. `path` is **village-root-relative**,
/// `/`-separated, with no `.`/`..` components (the daemon normalizes
/// before classification; [`FsEvent::new`] enforces it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    /// What happened.
    pub action: FsAction,
    /// Village-root-relative path.
    pub path: PathBuf,
    /// True when the path is known to be a directory.
    pub is_dir: bool,
}

impl FsEvent {
    /// Build an event, rejecting absolute paths and `.`/`..` components.
    ///
    /// # Errors
    ///
    /// `None` when `path` is absolute or contains a cur/parent component.
    #[must_use]
    pub fn new(action: FsAction, path: impl Into<PathBuf>, is_dir: bool) -> Option<Self> {
        let path = path.into();
        if path.is_absolute() {
            return None;
        }
        if path.components().any(|c| {
            matches!(
                c,
                Component::CurDir | Component::ParentDir | Component::RootDir
            )
        }) {
            return None;
        }
        Some(Self {
            action,
            path,
            is_dir,
        })
    }
}

/// What the governor side must do about an event (ADR-0019 cl. 5).
/// Handlers register per variant in the daemon dispatcher; `Ignored` needs
/// no handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    /// Nothing to do (unmapped region, hidden path, removal).
    Ignored,
    /// New incoming mail in `courier/mail/<recipient>/<file>` — route via
    /// the Sprint 03 delivery queue, then ack-rename (ADR-0019 cl. 3).
    MailSpool {
        /// Spool owner (recipient) dir name.
        recipient: SafeName,
        /// The `.mail` file name.
        file: SafeName,
    },
    /// `parcel<uid>/DEED.md` changed — Sprint 06 promotion consumer.
    DeedMutation {
        /// Parcel owner UID.
        uid: u32,
    },
    /// A Mayor decree emission (region match; Sprint 05 defines the
    /// encodings and interprets `path`).
    DecreeWrite {
        /// Village-relative path of the changed file.
        path: PathBuf,
    },
    /// New dispatch request in `courier/dispatch/outbox/` — Sprint 04
    /// step 4 sends it per ADR-0020.
    DispatchOutbox {
        /// Correlation id parsed from the `<correlation>.json` file name
        /// (ADR-0020 cl. 1). Charset-safe but not a single-extension
        /// [`SafeName`], so it travels as the typed id.
        correlation: CorrelationId,
    },
    /// New freight sandbox dir — ACL audit (Sprint 04 step 3).
    FreightDir {
        /// Directory name under `courier/freight/`.
        name: SafeName,
    },
}

/// Classify one debounced event against the ADR-0019 cl. 5 table. Pure.
#[must_use]
pub fn classify(event: &FsEvent) -> Reaction {
    let Some(owned) = clean_parts(&event.path) else {
        return Reaction::Ignored;
    };
    let parts: Vec<&str> = owned.iter().map(String::as_str).collect();
    match parts.as_slice() {
        // courier/mail/<recipient>/<file>
        ["courier", "mail", recipient, file]
            if event.action == FsAction::Created && !event.is_dir =>
        {
            match (
                sanitize_name(recipient),
                sanitize_name(file),
                crate::mail::is_incoming(file),
            ) {
                (Ok(recipient), Ok(file), true) => Reaction::MailSpool { recipient, file },
                _ => Reaction::Ignored,
            }
        }
        // courier/dispatch/outbox/<file>.json
        ["courier", "dispatch", "outbox", file]
            if event.action == FsAction::Created && !event.is_dir =>
        {
            match crate::dispatch::correlation_from_file_name(file) {
                Ok(correlation) => Reaction::DispatchOutbox { correlation },
                Err(_) => Reaction::Ignored,
            }
        }
        // courier/freight/<name>/ (dir creation only)
        ["courier", "freight", name] if event.action == FsAction::Created && event.is_dir => {
            match sanitize_name(name) {
                Ok(name) => Reaction::FreightDir { name },
                Err(_) => Reaction::Ignored,
            }
        }
        // keep/census/parcel<uid>/DEED.md
        ["keep", "census", parcel, "DEED.md"]
            if matches!(event.action, FsAction::Created | FsAction::Modified) && !event.is_dir =>
        {
            match parcel_uid(parcel) {
                Some(uid) => Reaction::DeedMutation { uid },
                None => Reaction::Ignored,
            }
        }
        // keep/census/manor/home/** — Mayor decree emissions (Sprint 05
        // interprets; the encoding ADR may extend this table).
        ["keep", "census", "manor", "home", rest @ ..]
            if !rest.is_empty()
                && matches!(event.action, FsAction::Created | FsAction::Modified)
                && !event.is_dir =>
        {
            Reaction::DecreeWrite {
                path: event.path.clone(),
            }
        }
        // guild/ledger/commission<n>/INDENTURE.md — decomposition decrees.
        ["guild", "ledger", commission, "INDENTURE.md"]
            if matches!(event.action, FsAction::Created | FsAction::Modified) && !event.is_dir =>
        {
            if commission_uid(commission).is_some() {
                Reaction::DecreeWrite {
                    path: event.path.clone(),
                }
            } else {
                Reaction::Ignored
            }
        }
        _ => Reaction::Ignored,
    }
}

/// Split a relative path into `&str` components; `None` when any component
/// is hidden (dot-prefixed) or the path is empty/absolute-ish.
fn clean_parts(path: &Path) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(seg) = component else {
            return None;
        };
        let seg = seg.to_str()?;
        if seg.is_empty() || seg.starts_with('.') {
            return None;
        }
        parts.push(seg.to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

/// `parcel<uid>` / uid extraction with slot-range check.
fn parcel_uid(segment: &str) -> Option<u32> {
    let uid: u32 = segment.strip_prefix("parcel")?.parse().ok()?;
    crate::mail::is_slot_uid(uid).then_some(uid)
}

/// `commission<n>` extraction (n ≥ 1, u64 like the tree accessors).
fn commission_uid(segment: &str) -> Option<u64> {
    let n: u64 = segment.strip_prefix("commission")?.parse().ok()?;
    (n >= 1).then_some(n)
}

// ---------------------------------------------------------------------------
// Debounce batch (ADR-0019 cl. 6)
// ---------------------------------------------------------------------------

/// Per-path event merge buffer for one quiesce window. Deterministic:
/// entries live in a [`BTreeMap`] keyed by relative path, so `drain` is
/// sorted-path order regardless of arrival order (CONVENTIONS §3.1).
///
/// Merge rules live in [`merge_actions`] (shared with the daemon's
/// per-file quiesce buffer so both fold events identically).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EventBatch {
    events: BTreeMap<PathBuf, FsEvent>,
}

impl EventBatch {
    /// Empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge one event into the batch (called by the daemon when a path's
    /// quiesce window elapses; earlier same-path events fold in).
    pub fn insert(&mut self, event: FsEvent) {
        match self.events.get(&event.path) {
            None => {
                self.events.insert(event.path.clone(), event);
            }
            Some(existing) => {
                let merged = merge_actions(existing.action, event.action);
                match merged {
                    Some(action) => {
                        self.events.insert(
                            event.path.clone(),
                            FsEvent {
                                action,
                                path: event.path.clone(),
                                is_dir: event.is_dir,
                            },
                        );
                    }
                    None => {
                        self.events.remove(&event.path);
                    }
                }
            }
        }
    }

    /// Number of distinct paths currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// True when nothing is buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Take all buffered events in sorted-path order (the daemon assigns
    /// monotonic sequence numbers in this order).
    #[must_use]
    pub fn drain(&mut self) -> Vec<FsEvent> {
        std::mem::take(&mut self.events).into_values().collect()
    }
}

/// Merge two same-path actions inside one quiesce window (ADR-0019 cl. 6);
/// `None` means the pair cancels out (nothing to dispatch). Pure — shared
/// by [`EventBatch::insert`] and the daemon's per-file debounce buffer so
/// both fold events by the same table:
///
/// - Created + Modified → Created (the file appeared; content is final by
///   quiesce).
/// - Created + Removed → `None` (blipped out of existence; nothing to do).
/// - Modified + Removed → Removed.
/// - Removed + Created → Created (re-created inside the window).
/// - anything else → the incoming action (last-writer semantics).
#[must_use]
pub fn merge_actions(existing: FsAction, incoming: FsAction) -> Option<FsAction> {
    match (existing, incoming) {
        (FsAction::Created, FsAction::Modified) => Some(FsAction::Created),
        (FsAction::Created, FsAction::Removed) => None,
        (FsAction::Modified, FsAction::Removed) => Some(FsAction::Removed),
        (_, FsAction::Created) => Some(FsAction::Created),
        (_, action) => Some(action),
    }
}

// ---------------------------------------------------------------------------
// Reaction-table unit tests (ADR-0019 cl. 5 golden mapping)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(action: FsAction, path: &str, is_dir: bool) -> FsEvent {
        FsEvent::new(action, path, is_dir).expect("relative clean path")
    }

    fn name(s: &str) -> SafeName {
        sanitize_name(s).unwrap()
    }

    /// The full ADR-0019 cl. 5 table, row by row.
    #[test]
    fn reaction_table_golden() {
        // mail spool additions
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/mail/1001/000000000007-1042-ab12.mail",
                false
            )),
            Reaction::MailSpool {
                recipient: name("1001"),
                file: name("000000000007-1042-ab12.mail"),
            }
        );
        // only Created counts; Modified/Removed are noise (rename-consumed)
        assert_eq!(
            classify(&ev(FsAction::Modified, "courier/mail/1001/x.mail", false)),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(FsAction::Removed, "courier/mail/1001/x.mail", false)),
            Reaction::Ignored
        );
        // non-.mail and hidden files are not incoming
        assert_eq!(
            classify(&ev(FsAction::Created, "courier/mail/1001/notes.txt", false)),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/mail/1001/.tmp-ab.mail",
                false
            )),
            Reaction::Ignored
        );
        // ack renames into .read/ are invisible
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/mail/1001/.read/x.mail",
                false
            )),
            Reaction::Ignored
        );
        // dirs under the spool are not mail
        assert_eq!(
            classify(&ev(FsAction::Created, "courier/mail/1001/sub", true)),
            Reaction::Ignored
        );

        // DEED.md mutations (Sprint 06)
        assert_eq!(
            classify(&ev(
                FsAction::Modified,
                "keep/census/parcel1042/DEED.md",
                false
            )),
            Reaction::DeedMutation { uid: 1042 }
        );
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "keep/census/parcel1000/DEED.md",
                false
            )),
            Reaction::DeedMutation { uid: 1000 }
        );
        assert_eq!(
            classify(&ev(
                FsAction::Modified,
                "keep/census/parcel4242/DEED.md",
                false
            )),
            Reaction::Ignored // out of slot range
        );
        assert_eq!(
            classify(&ev(
                FsAction::Removed,
                "keep/census/parcel1042/DEED.md",
                false
            )),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(
                FsAction::Modified,
                "keep/census/parcel1042/TITLE.md",
                false
            )),
            Reaction::Ignored
        );

        // decree regions (Sprint 05)
        let decree = ev(
            FsAction::Created,
            "keep/census/manor/home/decrees/d-7.md",
            false,
        );
        assert_eq!(
            classify(&decree),
            Reaction::DecreeWrite {
                path: decree.path.clone()
            }
        );
        let indenture = ev(
            FsAction::Modified,
            "guild/ledger/commission3/INDENTURE.md",
            false,
        );
        assert_eq!(
            classify(&indenture),
            Reaction::DecreeWrite {
                path: indenture.path.clone()
            }
        );
        assert_eq!(
            classify(&ev(
                FsAction::Modified,
                "guild/ledger/commissionx/INDENTURE.md",
                false
            )),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(FsAction::Created, "keep/census/manor/home", true)),
            Reaction::Ignored // the dir itself, not a decree emission
        );
        assert_eq!(
            classify(&ev(FsAction::Removed, "keep/census/manor/home/x.md", false)),
            Reaction::Ignored
        );

        // dispatch outbox additions
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/dispatch/outbox/d-1-1000.1.json",
                false
            )),
            Reaction::DispatchOutbox {
                correlation: CorrelationId::parse("d-1-1000.1").unwrap()
            }
        );
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/dispatch/outbox/notes.txt",
                false
            )),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(
                FsAction::Modified,
                "courier/dispatch/outbox/d-1-1000.1.json",
                false
            )),
            Reaction::Ignored
        );
        // inbox/dead writes are daemon-owned, never reactions
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/dispatch/inbox/d-1-1000.1.json",
                false
            )),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(
                FsAction::Created,
                "courier/dispatch/dead/d-1-1000.1.json",
                false
            )),
            Reaction::Ignored
        );

        // freight dir creations
        assert_eq!(
            classify(&ev(FsAction::Created, "courier/freight/report-7", true)),
            Reaction::FreightDir {
                name: name("report-7")
            }
        );
        assert_eq!(
            classify(&ev(FsAction::Created, "courier/freight/report-7", false)),
            Reaction::Ignored // file, not a sandbox dir
        );
        assert_eq!(
            classify(&ev(FsAction::Created, "courier/freight/a/b", true)),
            Reaction::Ignored // nested — only top-level dirs are sandboxes
        );

        // unmapped regions
        assert_eq!(
            classify(&ev(FsAction::Modified, "charter.json", false)),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(FsAction::Created, "tavern/gossip/x.md", false)),
            Reaction::Ignored
        );
        assert_eq!(
            classify(&ev(FsAction::Created, "courier/queues/1001.jsonl", false)),
            Reaction::Ignored // daemon-owned spool, no self-trigger loops
        );
    }

    /// Hidden components anywhere make events invisible (writer temp files,
    /// ack dirs, daemon internals).
    #[test]
    fn hidden_paths_never_react() {
        for path in [
            ".hamlet-internal/x",
            "courier/.tmp/mail/1001/x.mail",
            "courier/mail/.hidden/x.mail",
            "keep/census/manor/home/.scratch/d.md",
        ] {
            assert_eq!(
                classify(&ev(FsAction::Created, path, false)),
                Reaction::Ignored,
                "{path} must be ignored"
            );
        }
    }

    /// FsEvent construction rejects absolute and traversal paths.
    #[test]
    fn event_construction_guards() {
        assert!(FsEvent::new(FsAction::Created, "/etc/passwd", false).is_none());
        assert!(FsEvent::new(FsAction::Created, "courier/../charter.json", false).is_none());
        assert!(FsEvent::new(FsAction::Created, "./courier/mail", false).is_none());
        assert!(FsEvent::new(FsAction::Created, "courier/mail/1001/x.mail", false).is_some());
    }

    /// Batch merge semantics + sorted-path drain order regardless of
    /// arrival order (determinism, ADR-0019 cl. 6).
    #[test]
    fn batch_merge_and_sorted_drain() {
        let mut batch = EventBatch::new();
        let p = |s: &str| PathBuf::from(s);

        // arrival order deliberately unsorted
        batch.insert(ev(FsAction::Created, "courier/mail/1002/b.mail", false));
        batch.insert(ev(FsAction::Created, "courier/mail/1001/a.mail", false));
        batch.insert(ev(FsAction::Created, "courier/freight/zeta", true));

        // Created + Modified collapses to Created
        batch.insert(ev(FsAction::Modified, "courier/mail/1001/a.mail", false));
        // Created + Removed cancels
        batch.insert(ev(FsAction::Created, "courier/mail/1003/blip.mail", false));
        batch.insert(ev(FsAction::Removed, "courier/mail/1003/blip.mail", false));
        // Modified + Removed → Removed
        batch.insert(ev(
            FsAction::Modified,
            "keep/census/parcel1042/DEED.md",
            false,
        ));
        batch.insert(ev(
            FsAction::Removed,
            "keep/census/parcel1042/DEED.md",
            false,
        ));
        // Removed + Created → Created
        batch.insert(ev(FsAction::Removed, "courier/freight/reborn", true));
        batch.insert(ev(FsAction::Created, "courier/freight/reborn", true));

        let drained = batch.drain();
        let paths: Vec<&Path> = drained.iter().map(|e| e.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![
                p("courier/freight/reborn"),
                p("courier/freight/zeta"),
                p("courier/mail/1001/a.mail"),
                p("courier/mail/1002/b.mail"),
                p("keep/census/parcel1042/DEED.md"),
            ]
        );
        let actions: Vec<FsAction> = drained.iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![
                FsAction::Created,
                FsAction::Created,
                FsAction::Created,
                FsAction::Created,
                FsAction::Removed,
            ]
        );
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
        assert!(batch.drain().is_empty());
    }

    #[test]
    fn quiesce_constant() {
        assert_eq!(QUIESCE_MS, 250);
    }

    /// The merge table, exhaustively (3×3) — golden contract shared by
    /// `EventBatch` and the daemon debouncer.
    #[test]
    fn merge_actions_table_golden() {
        use FsAction::{Created, Modified, Removed};
        assert_eq!(merge_actions(Created, Modified), Some(Created));
        assert_eq!(merge_actions(Created, Removed), None);
        assert_eq!(merge_actions(Created, Created), Some(Created));
        assert_eq!(merge_actions(Modified, Removed), Some(Removed));
        assert_eq!(merge_actions(Modified, Created), Some(Created));
        assert_eq!(merge_actions(Modified, Modified), Some(Modified));
        assert_eq!(merge_actions(Removed, Created), Some(Created));
        assert_eq!(merge_actions(Removed, Modified), Some(Modified));
        assert_eq!(merge_actions(Removed, Removed), Some(Removed));
    }
}
