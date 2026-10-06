//! Guarded real-podman freight ACL suite (CONVENTIONS §5; SPEC §4 freight
//! ACLs + §8.3; ADR-0021).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH
//! **and** the host carries the ACL tools the daemon drives through
//! `podman unshare` (`setfacl`/`getfacl`/`cat`); otherwise it skips with a
//! recorded reason. `#[ignore]`d so a plain `cargo test` never touches
//! podman; `scripts/ci.sh` step 5 runs it via
//! `cargo test --workspace -- --ignored podman` on capable hosts.
//!
//! Coverage (step-03 verify contract): agent A secures `/var/freight/
//! exchange` for agent B with real `setfacl` → B reads AND writes → C is
//! denied by perms in every direction (and cannot unlink the sandbox:
//! sticky freight root) → the real daemon audit (getfacl through the
//! userns) passes the clean sandbox and flags a deliberately
//! world-writable dir, a foreign-UID grant, and a symlink entry → the
//! quarantine flag is host-written 0444, agent-readable, never
//! agent-writable → repeated violations trip the Mayor-wake hook →
//! daemon promotion-read succeeds on ALL sandboxes regardless of ACL,
//! refusing symlink escapes, without touching any ACL.
//!
//! Unlike the SPEC §4 uid-policy suites, this one asserts nothing about the
//! daemon user's own host uid: the real-ACL reads go through `podman unshare`
//! against agent-owned sandboxes, which is exactly why it stays runnable on a
//! host where the test user is also the podman owner. See
//! `tests/support/mod.rs`.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::freight::ViolationKind;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::Provisioner;
use hamlet_daemon::freight::{
    AuditOutcome, FreightGuard, QuarantineFlag, RecordingWake, ESCALATION_THRESHOLD,
};
use hamlet_daemon::podman::{ExecOutput, PodmanError, PodmanRuntime, Runtime as _};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{AgentKey, Telemetry};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 93;
/// Container names are **per-run** (QA04-26): podman names are global to the
/// store, so two overlapping batteries provisioned each other's containers and
/// then failed inside an isolation assertion with text that reads like a
/// SPEC §4 defect. The constants are the stable *bases*; every container,
/// brief and cleanup below uses the pid-scoped name.
const AGENT_A_BASE: &str = "hamlet-freight-a"; // slot 1001
const AGENT_B_BASE: &str = "hamlet-freight-b"; // slot 1002
const AGENT_C_BASE: &str = "hamlet-freight-c"; // slot 1003
const UID_A: u32 = 1001;
const UID_B: u32 = 1002;

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

/// One buffered `podman exec` against a guard container.
async fn run(
    runtime: &PodmanRuntime,
    container: &str,
    cmd: &[&str],
) -> Result<ExecOutput, PodmanError> {
    let argv: Vec<String> = cmd.iter().map(|s| (*s).to_owned()).collect();
    runtime.exec(container, &argv, None).await
}

/// One `sh -c` script inside a guard container.
async fn sh(
    runtime: &PodmanRuntime,
    container: &str,
    script: &str,
) -> Result<ExecOutput, PodmanError> {
    run(runtime, container, &["sh", "-c", script]).await
}

/// CONVENTIONS §5 guard: `HAMLET_PODMAN_TESTS=1`, podman on PATH, and the
/// host ACL tooling the daemon drives through `podman unshare`
/// (ADR-0021 cl. 2 host prerequisite); otherwise skip with a recorded
/// reason.
fn podman_guard() -> Option<String> {
    if let Some(reason) = support::podman_env_guard() {
        return Some(reason);
    }
    let found = StdCommand::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !found {
        return Some("podman not on PATH".to_owned());
    }
    for tool in ["setfacl", "getfacl", "cat"] {
        let ok = StdCommand::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || StdCommand::new("sh")
                .args(["-c", &format!("command -v {tool} >/dev/null 2>&1")])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        if !ok {
            return Some(format!(
                "host tool {tool} not on PATH (ADR-0021 prerequisite)"
            ));
        }
    }
    None
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0)
}

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman + host acl tools (CONVENTIONS §5)"]
async fn podman_freight_acl_sandboxes_share_by_roster_and_audit() {
    if let Some(reason) =
        podman_guard().or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_freight_acl_sandboxes_share_by_roster_and_audit",
            &reason,
        );
        return;
    }

    // QA04-26: podman container names are global to the store, so two
    // overlapping batteries adopted (and `rm -f`ed) each other's parcels and
    // then failed inside isolation assertions with text that reads like a
    // SPEC §4 defect. Per-run names make concurrent runs coexist; the
    // battery-wide lock is the CI layer's half (`guarded-entry.sh`).
    let agent_a = support::scoped(AGENT_A_BASE);
    let agent_b = support::scoped(AGENT_B_BASE);
    let agent_c = support::scoped(AGENT_C_BASE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    for agent in [&agent_a, &agent_b, &agent_c] {
        let brief = tree.register_brief(&sanitize_name(agent).unwrap());
        fs::write(brief, format!("# {agent}\n\nGuarded-test brief.\n")).expect("brief");
    }

    let config = Config::default();
    let runtime =
        Arc::new(PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(2)));
    let provisioner = Provisioner::new(
        tree.clone(),
        config.clone(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    // Whatever happens, the guard containers must not leak.
    let cleanup = || {
        for name in [&agent_a, &agent_b, &agent_c] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
    };
    cleanup();

    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).expect("sled"));
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let wake = Arc::new(RecordingWake::new());
    let guard = Arc::new(FreightGuard::new(
        Arc::clone(&supervisor),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        Arc::clone(&wake) as Arc<dyn hamlet_daemon::freight::MayorWake>,
    ));

    let result = async {
        // --- provision three real parcels (slots 1001..1003) --------------
        for agent in [&agent_a, &agent_b, &agent_c] {
            provisioner
                .provision_parcel(&sanitize_name(agent).unwrap(), COMMISSION, &blueprint)
                .await
                .map_err(|e| PodmanError::invalid(e.to_string()))?;
        }

        // --- root ACL provisioning: every slot may create sandboxes -------
        let report = guard.ensure_root_acl().await.map_err(err)?;
        assert_eq!(
            report.granted,
            vec![UID_A, UID_B, 1003],
            "charter slots get u:<uid>:rwx root grants (ADR-0021 cl. 2)"
        );
        assert_eq!(
            mode_of(&tree.courier_freight()),
            0o1770,
            "sticky daemon-owned freight root (mask-slot group bits)"
        );
        // Idempotence: a second ensure grants nothing new.
        let again = guard.ensure_root_acl().await.map_err(err)?;
        assert!(again.granted.is_empty());

        // --- A secures a sandbox for B (real setfacl inside the container)
        let out = sh(
            &runtime,
            &agent_a,
            "mkdir /var/freight/exchange && \
             setfacl -m u::rwx,u:1002:rwx,o::--- /var/freight/exchange && \
             echo payload-from-a > /var/freight/exchange/payload.md",
        )
        .await?;
        assert_eq!(out.code, 0, "A creates + secures: {}", out.stderr_lossy());

        // --- B reads and writes through the grant --------------------------
        let out = sh(
            &runtime,
            &agent_b,
            "cat /var/freight/exchange/payload.md && \
             echo response-from-b > /var/freight/exchange/response.md",
        )
        .await?;
        assert_eq!(
            out.code,
            0,
            "B is the ACL recipient: {}",
            out.stderr_lossy()
        );
        assert_eq!(out.stdout_lossy().trim(), "payload-from-a");
        let out = sh(&runtime, &agent_a, "cat /var/freight/exchange/response.md").await?;
        assert_eq!(out.code, 0, "A reads B's reply (owner)");
        assert_eq!(out.stdout_lossy().trim(), "response-from-b");

        // --- C is denied in every direction (perms, not goodwill) ----------
        for denied in [
            "ls /var/freight/exchange",
            "cat /var/freight/exchange/payload.md",
            "touch /var/freight/exchange/forged",
            "setfacl -m u:1003:rwx /var/freight/exchange", // no ACL rewrite by a stranger
            "rm -rf /var/freight/exchange",                // sticky root: no cross-unlink
            "mv /var/freight/exchange /var/freight/stolen",
        ] {
            let out = sh(&runtime, &agent_c, denied).await?;
            assert_ne!(out.code, 0, "C must be denied `{denied}`");
        }
        // C still SEES the shared root (rwx root grant) — names are public,
        // contents are ACL-gated.
        let out = sh(&runtime, &agent_c, "ls /var/freight").await?;
        assert_eq!(out.code, 0, "freight root is listable for slots");
        assert!(out.stdout_lossy().contains("exchange"));

        // --- audit: the clean sandbox passes --------------------------------
        let outcome = guard.audit(&name("exchange")).await.map_err(err)?;
        assert_eq!(
            outcome,
            AuditOutcome::Clean { owner: UID_A },
            "A secured for B: sane per ADR-0021 cl. 3"
        );

        // --- audit: a deliberately world-writable dir is flagged ------------
        let out = sh(
            &runtime,
            &agent_a,
            "mkdir /var/freight/openbox && chmod 0777 /var/freight/openbox && \
             echo open-artifact > /var/freight/openbox/leak.md",
        )
        .await?;
        assert_eq!(out.code, 0, "{}", out.stderr_lossy());
        let outcome = guard.audit(&name("openbox")).await.map_err(err)?;
        match outcome {
            AuditOutcome::Flagged {
                owner,
                violations,
                escalated,
            } => {
                assert_eq!(owner, UID_A);
                assert!(!escalated, "first violation does not escalate");
                assert_eq!(
                    violations.iter().map(|v| v.kind).collect::<Vec<_>>(),
                    vec![ViolationKind::WorldWritable]
                );
            }
            other => panic!("expected Flagged, got {other:?}"),
        }
        // C (any agent) can READ the evidence but never write/unlink it.
        let flag_host = tree.courier_freight_quarantine_flag(&name("openbox"));
        assert_eq!(mode_of(&flag_host) & 0o777, 0o444, "flag is 0444");
        let flag = QuarantineFlag::parse(&fs::read(&flag_host).expect("flag readable"))
            .expect("flag parses");
        assert_eq!(flag.owner, UID_A);
        assert_eq!(flag.violations[0].kind, "world_writable");
        let out = sh(
            &runtime,
            &agent_c,
            "cat /var/freight/.quarantine/openbox.json",
        )
        .await?;
        assert_eq!(out.code, 0, "evidence is agent-readable");
        assert!(out.stdout_lossy().contains("world_writable"));
        for denied in [
            "echo forged > /var/freight/.quarantine/openbox.json",
            "rm /var/freight/.quarantine/openbox.json",
            "touch /var/freight/.quarantine/forged.json",
        ] {
            let out = sh(&runtime, &agent_c, denied).await?;
            assert_ne!(out.code, 0, "evidence is never agent-writable: `{denied}`");
        }
        // The sled trace landed under the owner's agent key.
        let traces = telemetry
            .recent_traces(
                AgentKey {
                    commission: COMMISSION,
                    uid: UID_A,
                },
                32,
            )
            .expect("traces");
        assert!(
            traces
                .iter()
                .any(|t| t.event == "freight-violation" && t.detail.contains("openbox")),
            "{traces:?}"
        );

        // --- audit: foreign-UID grant is flagged -----------------------------
        let out = sh(
            &runtime,
            &agent_a,
            "mkdir /var/freight/rogue && setfacl -m u:4242:rwx /var/freight/rogue",
        )
        .await?;
        assert_eq!(out.code, 0, "{}", out.stderr_lossy());
        let outcome = guard.audit(&name("rogue")).await.map_err(err)?;
        match outcome {
            AuditOutcome::Flagged { violations, .. } => assert_eq!(
                violations.iter().map(|v| v.kind).collect::<Vec<_>>(),
                vec![ViolationKind::ForeignUid]
            ),
            other => panic!("expected Flagged, got {other:?}"),
        }

        // --- audit: symlink entry is flagged without a listing ---------------
        let out = sh(&runtime, &agent_a, "ln -s /etc /var/freight/evil").await?;
        assert_eq!(out.code, 0, "{}", out.stderr_lossy());
        let outcome = guard.audit(&name("evil")).await.map_err(err)?;
        match outcome {
            AuditOutcome::Flagged {
                owner, violations, ..
            } => {
                assert_eq!(owner, 0);
                assert_eq!(violations[0].kind, ViolationKind::NotADirectory);
            }
            other => panic!("expected Flagged, got {other:?}"),
        }

        // --- repeated violations trip the Mayor-wake hook --------------------
        // openbox + rogue + evil already give owner 1001 two flags (evil's
        // flag records owner 0); one more A-owned violation reaches the
        // threshold for UID_A.
        let out = sh(
            &runtime,
            &agent_a,
            "mkdir /var/freight/leaky && chmod 0777 /var/freight/leaky",
        )
        .await?;
        assert_eq!(out.code, 0, "{}", out.stderr_lossy());
        let outcome = guard.audit(&name("leaky")).await.map_err(err)?;
        match outcome {
            AuditOutcome::Flagged { escalated, .. } => assert!(
                escalated,
                "third A-owned flag reaches ESCALATION_THRESHOLD={ESCALATION_THRESHOLD}"
            ),
            other => panic!("expected Flagged, got {other:?}"),
        }
        let events = wake.events();
        assert_eq!(events.len(), 1, "exactly one wake event");
        assert_eq!(events[0].owner, UID_A);
        assert_eq!(
            events[0].quarantined,
            vec!["leaky".to_owned(), "openbox".to_owned(), "rogue".to_owned()],
            "sorted file-as-truth evidence list"
        );

        // --- promotion-read: host-side capability over EVERY sandbox --------
        let bytes = guard
            .promotion_read(&name("exchange"), &name("payload.md"))
            .await
            .map_err(err)?;
        assert_eq!(
            bytes, b"payload-from-a\n",
            "0700+ACL sandbox: daemon reads anyway"
        );
        let bytes = guard
            .promotion_read(&name("openbox"), &name("leak.md"))
            .await
            .map_err(err)?;
        assert_eq!(bytes, b"open-artifact\n");
        // Symlink sandbox: refused before any read (SPEC §4 traversal).
        let e = guard
            .promotion_read(&name("evil"), &name("passwd"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                hamlet_daemon::freight::FreightError::SymlinkRefused { .. }
            ),
            "{e}"
        );
        // Promotion-read relaxed NOTHING: B still reads, C still denied, and
        // the audit still calls the sandbox clean.
        let out = sh(&runtime, &agent_b, "cat /var/freight/exchange/payload.md").await?;
        assert_eq!(out.code, 0, "agent-side ACL untouched by promotion-read");
        let out = sh(&runtime, &agent_c, "cat /var/freight/exchange/payload.md").await?;
        assert_ne!(out.code, 0, "C still denied after promotion-read");
        assert_eq!(
            guard.audit(&name("exchange")).await.map_err(err)?,
            AuditOutcome::Clean { owner: UID_A }
        );

        Ok::<(), PodmanError>(())
    }
    .await;

    cleanup();
    result.expect("freight ACL invariants hold inside real containers");

    // Host-side braces after teardown: flags survive, evidence is 0444.
    let flag_host = tree.courier_freight_quarantine_flag(&name("openbox"));
    assert_eq!(mode_of(&flag_host) & 0o777, 0o444);
}

fn err<E: std::fmt::Display>(e: E) -> PodmanError {
    PodmanError::invalid(e.to_string())
}
