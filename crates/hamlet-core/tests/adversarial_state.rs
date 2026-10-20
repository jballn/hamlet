//! Adversarial suite — charter atomicity, UID slots, resumability
//! (Sprint 01, prompt 05-qa-engineer, items 3, 4, 6).
//!
//! SPEC §11.1, §4; CONVENTIONS §3 (state contract); ADR-0004.
//!
//! Two tests in this file are EXPECTED TO FAIL until the corresponding
//! HANDOFF findings are fixed (QA does not fix product code):
//! - `concurrent_savers_never_tear_or_fail` → **F01-01** (torn charter
//!   published under concurrent `save_atomic`; deterministic temp path).
//! - `duplicate_identity_across_slots_must_violate_invariant` → **F01-04**
//!   (`validate()` accepts one identity holding two slots).

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use hamlet_core::charter::{Charter, CharterError, ParcelLease};
use hamlet_core::commission::CommissionRecord;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::{self, SlotError, SlotRecord, SlotStatus, AGENT_SLOT_CAPACITY, MAYOR_UID};
use hamlet_core::tree::HamletTree;

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap_or_else(|e| panic!("test name {s:?} must sanitize: {e}"))
}

// ---------------------------------------------------------------------------
// Item 3 — charter atomicity
// ---------------------------------------------------------------------------

/// A REAL `kill -9` (SIGKILL, no destructors, no flush) landing anywhere in
/// the `save_atomic` loop must never leave a loadable-but-torn charter:
/// after the kill, `charter.json` is always one complete committed version,
/// a stale temp beside it is ignored by `load`, and the next save recovers.
///
/// Child phase (env `HAMLET_CRASH_CHILD=1`): load, then loop
/// {allocate one slot, save_atomic, print iteration} ~1ms apart until
/// SIGKILLed by the parent. A save failure would panic (exit 101) and the
/// parent's signal assertion would catch it.
#[test]
fn sigkill_mid_save_never_tears_charter() {
    if std::env::var_os("HAMLET_CRASH_CHILD").is_some() {
        let path = PathBuf::from(std::env::var_os("HAMLET_CRASH_PATH").unwrap());
        let mut charter = Charter::load(&path).unwrap();
        let mut i = 0u32;
        loop {
            i += 1;
            charter
                .allocate_slot(name(&format!("agent-{i}")), None)
                .unwrap();
            charter.save_atomic(&path).unwrap();
            println!("{i}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("charter.json");
    Charter::empty().save_atomic(&path).unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "sigkill_mid_save_never_tears_charter",
            "--exact",
            "--nocapture",
        ])
        .env("HAMLET_CRASH_CHILD", "1")
        .env("HAMLET_CRASH_PATH", &path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Let the child commit at least 25 versions, then SIGKILL it mid-flight.
    // The child's stdout also carries test-harness chatter — count only the
    // all-digit iteration lines it prints after each successful save.
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let mut last = 0u32;
    let mut seen = 0u32;
    while seen < 25 {
        let line = lines
            .next()
            .expect("child died before 25 committed saves")
            .unwrap();
        if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) {
            last = line.parse().unwrap();
            seen += 1;
        }
    }
    child.kill().unwrap(); // SIGKILL — no handler, no cleanup, no Drop
    let status = child.wait().unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "child must die by SIGKILL (a save failure would panic instead): {status:?}"
    );
    assert!(last >= 25);

    // The committed charter must be a COMPLETE version: parseable, valid,
    // and exactly a contiguous prefix of the child's allocation sequence.
    let recovered = Charter::load(&path).unwrap_or_else(|e| {
        panic!("charter torn by kill -9 mid-save (CONVENTIONS §3 rule 2): {e}")
    });
    let k = recovered.slots.len();
    assert!(
        k >= last as usize,
        "committed state regressed: {k} < {last}"
    );
    assert!(k <= 100_000);
    for (i, (&uid, record)) in recovered.slots.iter().enumerate() {
        assert_eq!(uid, 1001 + i as u32, "slot sequence corrupted at {i}");
        assert_eq!(record.identity, name(&format!("agent-{}", i + 1)));
    }
    // Canonical re-render is byte-identical to what is on disk.
    assert_eq!(recovered.render().as_bytes(), fs::read(&path).unwrap());

    // A stale temp file (kill between write and rename) is IGNORED by load
    // and cleaned up by the next save.
    let tmp = Charter::temp_path(&path);
    if tmp.exists() {
        // Whatever the torn temp holds, load already proved it is ignored.
        recovered.save_atomic(&path).unwrap();
        assert!(
            !tmp.exists(),
            "stale temp must be renamed away by next save"
        );
    }
    assert_eq!(Charter::load(&path).unwrap(), recovered);
}

/// **F01-01 — EXPECTED FAILURE until fixed.**
///
/// Two concurrent savers over one `charter.json` (two daemon components,
/// or daemon + CLI race). CONVENTIONS §3 rule 2 requires every write to be
/// atomic; SPEC §11.1 requires charter.json to be the master state map.
///
/// EXPECTED: every `save_atomic` returns `Ok` and `charter.json` always
/// loads as one of the two savers' complete states (last rename wins).
/// ACTUAL (repro: this test, ~1 in 4 rounds shows an error; torn publish
/// observed ~1 in 400): `save_atomic` uses the DETERMINISTIC temp path
/// `.charter.json.tmp` (`charter.rs` `Charter::temp_path`), so concurrent
/// savers share one temp file — interleavings produce (a) spurious
/// `Io { NotFound }` when the peer renamed our temp away, (b) `Io` errors
/// from the best-effort `remove_file` cleanup racing the peer's rename, and
/// (c) worst case a TORN charter published to `charter.json` (observed:
/// `Corrupt { detail: "trailing characters" }` — saver A renamed the temp
/// while saver B was mid-write into the same file).
/// The same deterministic-temp pattern exists in
/// `crates/hamlet-cli/src/init.rs` `atomic_write_inner` (concurrent inits).
#[test]
fn concurrent_savers_never_tear_or_fail() {
    const ROUNDS: usize = 300;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("charter.json");

    let mut a = Charter::empty();
    a.reserve_mayor(name("manor")).unwrap();
    a.allocate_slot(name("ada"), None).unwrap();
    let mut b = a.clone();
    b.allocate_slot(name("grace"), None).unwrap();
    assert_ne!(a, b);

    for round in 0..ROUNDS {
        a.save_atomic(&path).unwrap(); // committed baseline
        let (pa, pb) = (path.clone(), path.clone());
        let (ca, cb) = (a.clone(), b.clone());
        let h1 = std::thread::spawn(move || ca.save_atomic(&pa));
        let h2 = std::thread::spawn(move || cb.save_atomic(&pb));
        let (r1, r2) = (h1.join().unwrap(), h2.join().unwrap());

        assert!(
            r1.is_ok() && r2.is_ok(),
            "F01-01 round {round}: concurrent save_atomic must not fail; got r1={r1:?} r2={r2:?} \
             (deterministic temp path `.charter.json.tmp` is shared by both savers)"
        );
        let loaded = Charter::load(&path);
        let ok = matches!(&loaded, Ok(c) if *c == a || *c == b);
        assert!(
            ok,
            "F01-01 round {round}: charter.json must be a COMPLETE committed version; \
             got {loaded:?} (torn temp published by rename race)"
        );
        let _ = fs::remove_file(Charter::temp_path(&path)); // keep rounds independent
    }
}

/// Adversarial temp-file states beside a committed charter: torn half of
/// the NEXT version, a complete-but-stale older version, garbage, and a
/// DIRECTORY named `.charter.json.tmp`. `load` must ignore all of them and
/// the next `save_atomic` must recover (overwrite or fail cleanly without
/// corrupting the committed charter).
#[test]
fn load_ignores_hostile_temp_states() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("charter.json");

    let mut v2 = Charter::empty();
    v2.allocate_slot(name("ada"), None).unwrap();
    v2.save_atomic(&path).unwrap();
    let v2_bytes = fs::read(&path).unwrap();

    let tmp = Charter::temp_path(&path);
    let hostile_temps: Vec<(&str, Vec<u8>)> = vec![
        ("torn-half-of-next", {
            let mut v3 = v2.clone();
            v3.allocate_slot(name("grace"), None).unwrap();
            let r = v3.render();
            r.as_bytes()[..r.len() / 2].to_vec()
        }),
        ("garbage", b"not json at all \x00\xff".to_vec()),
        (
            "complete-stale",
            b"{\n  \"charter_version\": 1\n}\n".to_vec(),
        ),
    ];
    for (label, bytes) in hostile_temps {
        fs::write(&tmp, &bytes).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            v2_bytes,
            "temp write touched charter ({label})"
        );
        assert_eq!(
            Charter::load(&path).unwrap(),
            v2,
            "load must ignore hostile temp ({label})"
        );
        v2.save_atomic(&path).unwrap();
        assert!(!tmp.exists(), "save must consume the temp ({label})");
        assert_eq!(Charter::load(&path).unwrap(), v2);
    }

    // A DIRECTORY squatting on the temp name: save_atomic must fail with a
    // typed Io error and leave the committed charter intact (File::create
    // on a directory path errors; best-effort remove_file cannot remove a
    // dir — either outcome is acceptable ONLY if charter.json survives).
    fs::create_dir(&tmp).unwrap();
    let err = v2.save_atomic(&path).unwrap_err();
    assert!(matches!(err, CharterError::Io { .. }), "{err}");
    assert_eq!(
        Charter::load(&path).unwrap(),
        v2,
        "committed charter must survive"
    );
    fs::remove_dir(&tmp).unwrap();
    v2.save_atomic(&path).unwrap();
}

/// A failed save (read-only directory) must leave the previously committed
/// charter byte-identical and clean up its temp.
#[test]
fn failed_save_leaves_committed_charter_intact() {
    // Root bypasses permission bits — skip rather than false-pass.
    if let Ok(out) = Command::new("id").arg("-u").output() {
        if String::from_utf8_lossy(&out.stdout).trim() == "0" {
            eprintln!("SKIP: running as root; read-only dir not enforceable");
            return;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("charter.json");
    let v1 = Charter::empty();
    v1.save_atomic(&path).unwrap();
    let v1_bytes = fs::read(&path).unwrap();

    let mut v2 = v1.clone();
    v2.allocate_slot(name("ada"), None).unwrap();

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let err = v2.save_atomic(&path);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(err, Err(CharterError::Io { .. })), "{err:?}");
    assert_eq!(
        fs::read(&path).unwrap(),
        v1_bytes,
        "failed save must not touch the committed file"
    );
    assert!(
        !Charter::temp_path(&path).exists(),
        "failed save must clean its temp"
    );
    assert_eq!(Charter::load(&path).unwrap(), v1);
}

/// Corrupted JSON → hard error carrying line, column, and an ACCURATE byte
/// offset into the file (ADR-0004 rule 4; never silent recovery).
#[test]
fn corrupt_json_reports_accurate_byte_offset() {
    // Interior syntax error on line 1: offset lands exactly on the bad byte.
    let bytes = br#"{"charter_version": 1, "slots": ???}"#;
    match Charter::from_bytes(bytes).unwrap_err() {
        CharterError::Corrupt {
            line,
            column,
            byte_offset,
            detail,
        } => {
            assert_eq!((line, column), (1, 33));
            assert_eq!(byte_offset, bytes.iter().position(|&b| b == b'?').unwrap());
            assert_eq!(
                bytes[byte_offset], b'?',
                "offset must point AT the offending byte: {detail}"
            );
        }
        other => panic!("expected Corrupt, got {other}"),
    }

    // Interior error on line 3 of a multi-line file.
    let text = "{\n  \"charter_version\": 1,\n  \"slots\": @\n}\n";
    let bytes = text.as_bytes();
    match Charter::from_bytes(bytes).unwrap_err() {
        CharterError::Corrupt {
            line, byte_offset, ..
        } => {
            assert_eq!(line, 3);
            assert_eq!(byte_offset, bytes.iter().position(|&b| b == b'@').unwrap());
            assert_eq!(bytes[byte_offset], b'@');
        }
        other => panic!("expected Corrupt, got {other}"),
    }

    // Trailing characters after a complete document.
    let bytes = br#"{"charter_version": 1} trailing-garbage"#;
    match Charter::from_bytes(bytes).unwrap_err() {
        CharterError::Corrupt { byte_offset, .. } => {
            assert_eq!(
                bytes[byte_offset], b't',
                "offset must point at the trailing garbage"
            );
        }
        other => panic!("expected Corrupt, got {other}"),
    }

    // Truncation clamps to EOF (line past end of file).
    let bytes = b"{\n  \"charter_version\": 1,\n";
    match Charter::from_bytes(bytes).unwrap_err() {
        CharterError::Corrupt { byte_offset, .. } => assert_eq!(byte_offset, bytes.len()),
        other => panic!("expected Corrupt, got {other}"),
    }

    // Invalid UTF-8: offset still points at the offending byte.
    let mut bytes = vec![b'{', 0xFF];
    bytes.extend_from_slice(b" \"charter_version\": 1 }");
    match Charter::from_bytes(&bytes).unwrap_err() {
        CharterError::Corrupt { byte_offset, .. } => assert_eq!(bytes[byte_offset], 0xFF),
        other => panic!("expected Corrupt, got {other}"),
    }

    // Empty file / bare array / wrong type: all Corrupt, never a panic.
    for bytes in [
        &b""[..],
        b"[]",
        b"null",
        b"{\"charter_version\": \"one\"}",
        br#"{"charter_version": 1, "slots": {"1001": {"identity": "../evil", "status": "active"}}}"#,
        br#"{"charter_version": 1, "slots": {"1001": {"identity": "ada", "status": "frozen"}}}"#,
        br#"{"charter_version": 1, "slots": {"1001": {"identity": "ada", "status": "active", "x": 1}}}"#,
    ] {
        assert!(
            matches!(Charter::from_bytes(bytes), Err(CharterError::Corrupt { .. })),
            "must be Corrupt: {}",
            String::from_utf8_lossy(bytes)
        );
    }

    // Version gate beats corruption detail for future versions.
    assert!(matches!(
        Charter::from_bytes(br#"{"charter_version": 99, "slots": ###}"#),
        Err(CharterError::Corrupt { .. })
    ));
    assert!(matches!(
        Charter::from_bytes(br#"{"charter_version": 99}"#),
        Err(CharterError::UnsupportedVersion { found: 99, .. })
    ));
}

// ---------------------------------------------------------------------------
// Item 4 — UID slots, adversarial
// ---------------------------------------------------------------------------

/// 1000 allocations fill 1001–2000; the 1001st IDENTITY is a typed hard
/// error — through the Charter API, persisted, and still exhausted after a
/// fresh reload (SPEC §4: exhaustion is surfaced, never wrapped).
#[test]
fn slot_exhaustion_at_1001st_identity_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("charter.json");
    let mut charter = Charter::empty();
    charter.reserve_mayor(name("manor")).unwrap();
    for i in 0..AGENT_SLOT_CAPACITY {
        let uid = charter
            .allocate_slot(name(&format!("agent-{i}")), None)
            .unwrap();
        assert_eq!(uid, 1001 + i as u32, "allocation must be sequential");
    }
    assert_eq!(
        charter.allocate_slot(name("one-too-many"), None),
        Err(SlotError::Exhausted {
            identity: name("one-too-many")
        })
    );
    // The refused allocation left NO partial state.
    assert_eq!(charter.slots.len(), AGENT_SLOT_CAPACITY + 1); // + mayor
    assert!(charter.validate().is_ok());
    charter.save_atomic(&path).unwrap();

    // Fresh reload: still exhausted, still no wrap, mayor intact.
    let mut reloaded = Charter::load(&path).unwrap();
    assert_eq!(reloaded.slots.len(), AGENT_SLOT_CAPACITY + 1);
    assert_eq!(
        reloaded.allocate_slot(name("one-too-many"), None),
        Err(SlotError::Exhausted {
            identity: name("one-too-many")
        })
    );
    // An EXISTING identity still resolves to its permanent slot when full.
    assert_eq!(reloaded.allocate_slot(name("agent-0"), None).unwrap(), 1001);
    assert_eq!(reloaded.slots[&MAYOR_UID].identity, name("manor"));
    assert_eq!(slots::next_free_uid(&reloaded.slots), None);
}

/// A hibernated (or crashed) slot is NEVER handed to a new identity — even
/// when it is the lowest free-looking uid — and the same identity always
/// maps back to its original slot (SPEC §4/§10; CONVENTIONS §3 rule 5).
#[test]
fn hibernated_and_crashed_slots_are_never_reused() {
    let mut charter = Charter::empty();
    let ada = charter.allocate_slot(name("ada"), None).unwrap();
    let grace = charter.allocate_slot(name("grace"), None).unwrap();
    assert_eq!((ada, grace), (1001, 1002));

    slots::hibernate(&mut charter.slots, ada).unwrap();
    slots::mark_crashed(&mut charter.slots, grace).unwrap();

    // New identities skip BOTH parked slots.
    let alan = charter.allocate_slot(name("alan"), None).unwrap();
    let bo = charter.allocate_slot(name("bo"), None).unwrap();
    assert_eq!((alan, bo), (1003, 1004));
    assert_eq!(charter.slots[&ada].identity, name("ada"));
    assert_eq!(charter.slots[&ada].status, SlotStatus::Hibernated);
    assert_eq!(charter.slots[&grace].status, SlotStatus::Crashed);

    // Same identities map back to their ORIGINAL slots; allocation does not
    // silently flip status (reactivation is an explicit transition).
    assert_eq!(charter.allocate_slot(name("ada"), None).unwrap(), ada);
    assert_eq!(charter.slots[&ada].status, SlotStatus::Hibernated);
    assert_eq!(charter.allocate_slot(name("grace"), None).unwrap(), grace);
    assert_eq!(charter.slots[&grace].status, SlotStatus::Crashed);
    slots::reactivate(&mut charter.slots, ada).unwrap();
    assert_eq!(charter.slots[&ada].status, SlotStatus::Active);
}

/// Double-allocation of one identity is idempotent: same uid, no duplicate
/// record, and the existing commission ref / status are NOT overwritten.
#[test]
fn double_allocation_is_idempotent_not_duplicated() {
    let mut charter = Charter::empty();
    let first = charter.allocate_slot(name("ada"), Some(7)).unwrap();
    charter
        .commissions
        .insert(7, CommissionRecord::draft(name("commission")));
    let before = charter.slots.clone();

    let again = charter.allocate_slot(name("ada"), Some(999)).unwrap();
    assert_eq!(again, first);
    assert_eq!(
        charter.slots, before,
        "idempotent allocation must not mutate the record"
    );
    assert_eq!(
        charter.slots[&first].commission,
        Some(7),
        "commission ref must not be overwritten"
    );
    assert_eq!(
        charter
            .slots
            .values()
            .filter(|r| r.identity == name("ada"))
            .count(),
        1,
        "identity must appear in exactly one slot"
    );

    // Mayor reservation is likewise idempotent, and conflict is typed.
    assert_eq!(charter.reserve_mayor(name("manor")).unwrap(), MAYOR_UID);
    assert_eq!(charter.reserve_mayor(name("manor")).unwrap(), MAYOR_UID);
    assert_eq!(
        charter.reserve_mayor(name("impostor")),
        Err(SlotError::MayorConflict {
            holder: name("manor"),
            attempted: name("impostor")
        })
    );
    assert_eq!(charter.slots[&MAYOR_UID].identity, name("manor"));
}

/// Lying charters: crafted slot records with out-of-policy uids or unsafe
/// identities must be rejected at load (ADR-0004 fail-hard).
#[test]
fn crafted_slot_records_are_rejected_at_load() {
    for uid in [0u32, 1, 999, 2001, u32::MAX] {
        let mut charter = Charter::empty();
        charter
            .slots
            .insert(uid, SlotRecord::new(name("squatter"), None));
        assert!(
            matches!(charter.validate(), Err(CharterError::Invariant { .. })),
            "uid {uid} outside 1000/1001-2000 must violate invariants"
        );
        let bytes = charter.render();
        assert!(
            matches!(
                Charter::from_bytes(bytes.as_bytes()),
                Err(CharterError::Invariant { .. })
            ),
            "crafted charter with uid {uid} must not load"
        );
    }
    // Unsafe identity via JSON is caught by SafeName re-validation.
    let lying =
        br#"{"charter_version":1,"slots":{"1001":{"identity":"../../etc","status":"active"}}}"#;
    assert!(matches!(
        Charter::from_bytes(lying),
        Err(CharterError::Corrupt { .. })
    ));
    // Cross-group lies.
    let lying = br#"{"charter_version":1,"parcels":{"1001":{"identity":"ada","stasis":false}}}"#;
    assert!(matches!(
        Charter::from_bytes(lying),
        Err(CharterError::Invariant { .. })
    ));
    let lying = br#"{"charter_version":1,"slots":{"1001":{"identity":"ada","status":"active","commission":42}}}"#;
    assert!(matches!(
        Charter::from_bytes(lying),
        Err(CharterError::Invariant { .. })
    ));
}

/// **F01-04 — EXPECTED FAILURE until fixed.**
///
/// A corrupt/hand-edited charter binding ONE identity to TWO slots violates
/// SPEC §4 ("allocated sequentially and permanently per agent identity")
/// and ADR-0004 (fail-hard load): `slots::allocate`'s identity lookup
/// becomes ambiguous (returns the lowest uid), hibernation of "the" slot is
/// undefined, and `next_free_uid` bookkeeping drifts.
///
/// EXPECTED: `Charter::validate` rejects duplicate identities across slots
/// with `CharterError::Invariant`.
/// ACTUAL: the charter loads fine (`from_bytes` → Ok) — `validate()` in
/// `crates/hamlet-core/src/charter.rs` checks parcel↔slot and agent↔slot
/// identity matches but never slot-table identity uniqueness.
#[test]
fn duplicate_identity_across_slots_must_violate_invariant() {
    let lying = br#"{
  "charter_version": 1,
  "slots": {
    "1001": { "identity": "ada", "status": "active" },
    "1002": { "identity": "ada", "status": "active" }
  }
}"#;
    let loaded = Charter::from_bytes(lying);
    assert!(
        matches!(loaded, Err(CharterError::Invariant { .. })),
        "F01-04: duplicate identity across slots must be an Invariant error; got {loaded:?}"
    );
    // Same lie constructed in memory must fail validate() too.
    let mut charter = Charter::empty();
    charter
        .slots
        .insert(1001, SlotRecord::new(name("ada"), None));
    charter
        .slots
        .insert(1002, SlotRecord::new(name("ada"), None));
    assert!(
        matches!(charter.validate(), Err(CharterError::Invariant { .. })),
        "F01-04: in-memory duplicate identity must fail validate()"
    );
}

// ---------------------------------------------------------------------------
// Item 6 — resumability: full cycle, fresh loads from disk only
// ---------------------------------------------------------------------------

/// Full cycle — init (tree + empty charter on disk), allocate, save,
/// simulated daemon restart (a FRESH `Charter::load` from disk; no in-memory
/// state carried over) → identical state, byte-identical canonical render,
/// and allocation continues coherently (existing identity → same uid; new
/// identity → next sequential uid).
#[test]
fn full_cycle_restart_from_disk_only_is_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();

    // "init": empty versioned charter on disk (what `hamlet init` writes).
    Charter::empty().save_atomic(&tree.charter()).unwrap();

    // --- daemon run 1: load from disk only, mutate, save, "crash" ---
    let mut c1 = Charter::load(&tree.charter()).unwrap();
    assert_eq!(c1, Charter::empty());
    c1.reserve_mayor(name("manor")).unwrap();
    let ada = c1.allocate_slot(name("ada"), Some(1)).unwrap();
    let grace = c1.allocate_slot(name("grace"), None).unwrap();
    c1.commissions
        .insert(1, CommissionRecord::draft(name("commission")));
    c1.parcels.insert(
        ada,
        ParcelLease {
            identity: name("ada"),
            stasis: false,
        },
    );
    slots::hibernate(&mut c1.slots, grace).unwrap();
    c1.parcels.insert(
        grace,
        ParcelLease {
            identity: name("grace"),
            stasis: true,
        },
    );
    c1.save_atomic(&tree.charter()).unwrap();
    let committed = fs::read(tree.charter()).unwrap();
    let expected = c1.clone();
    drop(c1); // kill -9 equivalent: no flush, no Drop ceremony

    // --- daemon run 2: fresh load from disk only ---
    let mut c2 = Charter::load(&tree.charter()).unwrap();
    assert_eq!(c2, expected, "restart must recover identical state");
    assert_eq!(
        c2.render().as_bytes(),
        committed,
        "canonical re-render must be byte-identical after restart"
    );
    // Hibernated slot survived with its mapping (SPEC §10).
    assert_eq!(c2.slots[&grace].identity, name("grace"));
    assert_eq!(c2.slots[&grace].status, SlotStatus::Hibernated);

    // Allocation continues coherently across the restart.
    assert_eq!(
        c2.allocate_slot(name("ada"), None).unwrap(),
        ada,
        "permanence"
    );
    assert_eq!(
        c2.allocate_slot(name("grace"), None).unwrap(),
        grace,
        "hibernated identity keeps slot"
    );
    let alan = c2.allocate_slot(name("alan"), None).unwrap();
    assert_eq!(alan, 1003, "sequential allocation resumes after restart");
    c2.save_atomic(&tree.charter()).unwrap();

    // --- daemon run 3 ---
    let c3 = Charter::load(&tree.charter()).unwrap();
    assert_eq!(c3, c2);
    assert_eq!(c3.slots.len(), 4); // mayor + ada + grace + alan
    assert!(c3.validate().is_ok());
}

/// State derivation reads no wall clock: two charters built through the
/// same operation sequence are byte-identical regardless of when they were
/// built (CONVENTIONS §3 rule 4 — determinism check).
#[test]
fn identical_operations_produce_identical_bytes() {
    fn build() -> String {
        let mut c = Charter::empty();
        c.reserve_mayor(name("manor")).unwrap();
        for i in 0..50 {
            c.allocate_slot(name(&format!("agent-{i}")), Some(i % 3))
                .unwrap();
        }
        for i in 0..3 {
            c.commissions
                .insert(i, CommissionRecord::draft(name("commission")));
        }
        slots::hibernate(&mut c.slots, 1010).unwrap();
        c.render()
    }
    let a = build();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b = build();
    assert_eq!(a, b, "state derivation must be time-independent");
    // Sorted-key sanity: slot keys render in ascending NUMERIC-as-string
    // order (BTreeMap<u32,...> serialized via to_value → string keys sorted
    // lexicographically is NOT required; to_value keeps numeric key order
    // for u32 keys — pin the contract either way).
    let parsed: BTreeMap<String, serde_json::Value> = serde_json::from_str(&a).unwrap();
    let slots = parsed
        .get("slots")
        .and_then(|s| s.as_object())
        .expect("slots group");
    let keys: Vec<&String> = slots.keys().collect();
    let mut sorted_lex = keys.clone();
    sorted_lex.sort();
    assert_eq!(
        keys, sorted_lex,
        "charter JSON must have sorted keys at every level"
    );
}

/// The charter file path itself must come from the typed tree accessor —
/// sanity check that `save_atomic`+`load` agree with `HamletTree::charter`
/// and that no stray temp artifacts remain inside the normative tree after
/// a full cycle (file-as-truth: the tree contains only tree files).
#[test]
fn no_temp_litter_inside_village_after_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut c = Charter::empty();
    c.reserve_mayor(name("manor")).unwrap();
    for round in 0..10 {
        c.allocate_slot(name(&format!("agent-{round}")), None)
            .unwrap();
        c.save_atomic(&tree.charter()).unwrap();
    }
    let mut entries: Vec<String> = fs::read_dir(tree.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert!(
        !entries.iter().any(|n| n.ends_with(".tmp")),
        "temp litter inside village root: {entries:?}"
    );
    assert_eq!(Charter::load(&tree.charter()).unwrap(), c);
    assert!(tree.charter().is_file());
}

/// Fix-pass pin (ADR-0008): identity uniqueness spans the WHOLE slot table
/// — including the mayor uid — and the error names the identity and uid.
#[test]
fn duplicate_identity_including_mayor_is_rejected() {
    // Same identity on the mayor slot and an agent slot.
    let lying = br#"{"charter_version": 1, "slots": {
      "1000": { "identity": "manor", "status": "active" },
      "1001": { "identity": "manor", "status": "active" } } }"#;
    assert!(
        matches!(
            Charter::from_bytes(lying),
            Err(CharterError::Invariant { .. })
        ),
        "identity duplicated across mayor+agent slots must be an Invariant error"
    );
    // Three-way duplicate: rejected, and the message names identity + uid.
    let lying = br#"{"charter_version": 1, "slots": {
      "1001": { "identity": "ada", "status": "active" },
      "1002": { "identity": "ada", "status": "active" },
      "1003": { "identity": "ada", "status": "crashed" } } }"#;
    match Charter::from_bytes(lying).unwrap_err() {
        CharterError::Invariant { message } => {
            assert!(
                message.contains("ada") && message.contains("1002"),
                "{message}"
            );
        }
        other => panic!("expected Invariant, got {other}"),
    }
    // A full 1000-slot table of UNIQUE identities still validates.
    let mut charter = Charter::empty();
    charter.reserve_mayor(name("manor")).unwrap();
    for i in 0..AGENT_SLOT_CAPACITY {
        charter
            .allocate_slot(name(&format!("agent-{i}")), None)
            .unwrap();
    }
    assert!(charter.validate().is_ok());
}
