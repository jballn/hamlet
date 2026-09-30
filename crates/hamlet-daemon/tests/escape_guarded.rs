//! QA 02/05 — guarded real-podman isolation & escape suite (Sprint 02 step 05).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH;
//! otherwise it skips with a recorded reason. `#[ignore]`d so a plain
//! `cargo test` never touches podman; `scripts/ci.sh` step 5 and
//! `scripts/e2e-02-isolation.sh` run it via `--ignored podman` on capable
//! hosts (CONVENTIONS §5).
//!
//! Extends — never duplicates — `provision_guarded.rs` (baseline ro/cross-UID
//! probes) and `bootstrap_guarded.rs` (toolchain + home secret scan) with the
//! step-05 adversarial angles:
//!
//! * **escape suite** (fast: parent image only, no toolchain downloads) —
//!   symlink-into-ro and hardlink-from-ro tricks, redirection through a
//!   shell, unprivileged remount attempts, spool forgery (`mkdir` in the
//!   0755 mail root), freight writes, the full infrastructure-socket matrix
//!   (podman/docker/dbus/systemd), no container tooling or host endpoint
//!   env, capability/NoNewPrivs state, `/proc/self/uid_map` analysis (the
//!   daemon's host uid must NOT fall inside the container's mapping), and
//!   per-slot host-uid distinctness (ADR-0011: home/spool owners differ per
//!   slot and never equal the daemon user).
//! * **bootstrap integrity suite** — `.env` secret material (obvious fake,
//!   CONVENTIONS §6) must appear in NO provisioned home, NO pkgcache mirror
//!   entry, NO derived-image layer (`podman history`/`inspect`), and no
//!   `Containerfile` may exist anywhere under the village tree (ADR-0012:
//!   the build context is daemon-private and outside the tree).
//!
//! SPEC citations: §4 (isolation invariants, UID policy, freight ACLs),
//! §11.4 (secrets); ADR-0010 (invocation policy), ADR-0011 (host permission
//! model + subuid userns), ADR-0012 (derived images), ADR-0013 (mirror).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::Provisioner;
use hamlet_daemon::hostperm::HostPermissions;
use hamlet_daemon::podman::{ExecOutput, PodmanRuntime, RunSpec, Runtime as _};

/// Obvious fake (CONVENTIONS §6) — never a real secret.
const FAKE_SECRET: &str = "test-key-do-not-use-QA02-escape";

/// CONVENTIONS §5 guard: run only with `HAMLET_PODMAN_TESTS=1` and podman
/// on PATH; otherwise skip with a recorded reason.
fn podman_guard() -> Option<String> {
    if std::env::var("HAMLET_PODMAN_TESTS").unwrap_or_default() != "1" {
        return Some("HAMLET_PODMAN_TESTS != 1".to_owned());
    }
    let found = StdCommand::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !found {
        return Some("podman not on PATH".to_owned());
    }
    None
}

async fn run(runtime: &PodmanRuntime, container: &str, cmd: &[&str]) -> Result<ExecOutput, String> {
    let argv: Vec<String> = cmd.iter().map(|s| (*s).to_owned()).collect();
    runtime
        .exec(container, &argv, None)
        .await
        .map_err(|e| e.to_string())
}

/// `sh -c` probe — TEST-side convenience only: product code never renders a
/// shell (ADR-0010 cl. 1); the escape suite uses it to model an adversarial
/// agent that DOES have a shell inside its parcel.
async fn sh(runtime: &PodmanRuntime, container: &str, script: &str) -> Result<ExecOutput, String> {
    run(runtime, container, &["sh", "-c", script]).await
}

fn host_uid() -> u32 {
    StdCommand::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .expect("host `id -u`")
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0)
}

fn owner_of(path: &Path) -> u32 {
    fs::metadata(path).map(|m| m.uid()).unwrap_or(u32::MAX)
}

/// Recursively give a subuid-owned path back to the daemon user so the
/// tempdir can be deleted (ADR-0011: slot homes/spools are 0700 subuid).
fn reclaim(path: &Path) {
    if path.exists() {
        let _ = StdCommand::new("podman")
            .args(["unshare", "chown", "-R", "0:0", "--"])
            .arg(path)
            .output();
    }
}

const COMMISSION: u64 = 95;
const AGENT_A: &str = "qa-escape-a"; // slot 1001
const AGENT_B: &str = "qa-escape-b"; // slot 1002
const UID_A: u32 = 1001;
const UID_B: u32 = 1002;

// ---------------------------------------------------------------------------
// 1. Escape suite: ro mounts, symlink/hardlink tricks, cross-UID, sockets,
//    capabilities, userns mapping (task items 1–4).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_escape_suite_ro_symlinks_cross_uid_no_infra_and_userns() {
    if let Some(reason) = podman_guard() {
        eprintln!("SKIP podman_escape_suite_ro_symlinks_cross_uid_no_infra_and_userns: {reason}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");

    let config = Config::default();
    let runtime = Arc::new(PodmanRuntime::from_config(&config));
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    // Mount sources + per-UID dirs, then the ADR-0011 permission braces.
    // The escape suite deliberately runs the PARENT image (no toolchain
    // bootstrap): isolation invariants must not depend on ~200 MB of tools.
    for uid in [UID_A, UID_B] {
        fs::create_dir_all(tree.parcel_home(uid)).expect("home dir");
        let spool = tree.courier_mail_recipient(&sanitize_name(&uid.to_string()).unwrap());
        fs::create_dir_all(&spool).expect("spool dir");
    }
    fs::create_dir_all(tree.commission_git(COMMISSION)).expect("git dir");
    fs::create_dir_all(tree.courier_freight()).expect("freight dir");
    fs::create_dir_all(tree.market_pkgcache()).expect("pkgcache dir");
    fs::create_dir_all(tree.tavern_gossip()).expect("gossip dir");
    // Read-only seed content: targets for the hardlink/symlink escape tries.
    fs::write(tree.commission_git(COMMISSION).join("seed.txt"), "ro\n").expect("git seed");
    fs::write(tree.tavern_gossip().join("seed.txt"), "ro\n").expect("gossip seed");

    let cleanup = || {
        for name in [AGENT_A, AGENT_B] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
        for uid in [UID_A, UID_B] {
            reclaim(&tree.parcel_home(uid));
            reclaim(&tree.courier_mail_recipient(&sanitize_name(&uid.to_string()).unwrap()));
        }
    };
    cleanup();

    let result = async {
        let perms = HostPermissions::new(runtime.as_ref());
        for uid in [UID_A, UID_B] {
            perms
                .apply_parcel_home(&tree, uid)
                .await
                .map_err(|e| e.to_string())?;
            perms
                .apply_mail_spool(&tree, uid)
                .await
                .map_err(|e| e.to_string())?;
        }
        perms
            .apply_shared(&tree, COMMISSION)
            .await
            .map_err(|e| e.to_string())?;

        runtime
            .pull(blueprint.meta().base_image())
            .await
            .map_err(|e| e.to_string())?;
        for (agent, uid) in [(AGENT_A, UID_A), (AGENT_B, UID_B)] {
            let spec = RunSpec::for_agent(
                &sanitize_name(agent).unwrap(),
                uid,
                COMMISSION,
                &blueprint,
                &config,
            )
            .map_err(|e| e.to_string())?;
            runtime
                .run_detached(&spec, &tree)
                .await
                .map_err(|e| e.to_string())?;
        }

        // --- task 4: userns mapping — inside uid == charter slot ----------
        let out = run(&runtime, AGENT_A, &["id", "-u"]).await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            UID_A.to_string(),
            "A runs as its slot"
        );
        let out = run(&runtime, AGENT_B, &["id", "-u"]).await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            UID_B.to_string(),
            "B runs as its slot"
        );

        // --- task 1: ro mounts, direct + every indirection we can model ---
        for denied in [
            "touch /var/git/escape",
            "mkdir /var/git/sub",
            "rm /var/git/seed.txt",
            "echo x > /var/git/redir",     // shell redirection
            "echo x > /var/context/redir", // the other ro mount
            "touch /var/context/escape",
            "rm /var/context/seed.txt",
            "ln /var/git/seed.txt /home/1001/hardlink", // hardlink OUT of ro
            "ln /var/context/seed.txt /home/1001/hardlink2",
            "ln -s /var/git /home/1001/gitlink && touch /home/1001/gitlink/escape", // symlink INTO ro
            "ln -s /var/context /home/1001/ctxlink && touch /home/1001/ctxlink/escape",
            "ln -s /var/git/seed.txt /home/1001/filelink && echo x > /home/1001/filelink",
            "mount -o remount,rw /var/git", // unprivileged remount
            "mount -o remount,rw /var/context",
            "chmod 777 /var/git",
            "chown 1001 /var/git/seed.txt",
        ] {
            let out = sh(&runtime, AGENT_A, denied).await?;
            assert_ne!(
                out.code, 0,
                "ro escape attempt must fail: `{denied}` (SPEC §4: agents cannot write \
                 /var/git/ or /var/context/)"
            );
        }
        // The symlink itself may be created in the rw home — but writing
        // THROUGH it must have failed and left nothing on the ro side.
        let out = sh(&runtime, AGENT_A, "ls /var/git /var/context").await?;
        assert!(
            !out.stdout_lossy().contains("escape"),
            "nothing landed in the ro mounts: {}",
            out.stdout_lossy()
        );

        // --- task 2: cross-UID home / spool / freight ----------------------
        for denied in [
            "ls /home/1002", // B's home: not mounted in A
            "cat /home/1002/anything",
            "ls /var/mail/1002", // B's spool: 0700 slot-owned
            "touch /var/mail/1002/forged",
            "cat /var/mail/1002/* 2>/dev/null; ls /var/mail/1002",
            "mkdir /var/mail/9999", // spool forgery in the 0755 root
            "touch /var/mail/rootfile",
            "ls /var/freight", // 0710 daemon: ACL-gated (Sprint 04)
            "mkdir /var/freight/sandbox",
            "touch /var/freight/x",
        ] {
            let out = sh(&runtime, AGENT_A, denied).await?;
            assert_ne!(
                out.code, 0,
                "cross-UID/infra write must fail: `{denied}` (SPEC §4)"
            );
        }
        // Own resources stay rw (isolation must not become self-DoS).
        for allowed in [
            "touch /home/1001/probe",
            "touch /var/mail/1001/probe",
            "touch /var/pkgcache/probe-a",
        ] {
            let out = sh(&runtime, AGENT_A, allowed).await?;
            assert_eq!(
                out.code,
                0,
                "own resource must stay rw: `{allowed}`: {}",
                out.stderr_lossy()
            );
        }
        // Mirror from B.
        for denied in [
            "ls /home/1001",
            "ls /var/mail/1001",
            "touch /var/mail/1001/forged",
        ] {
            let out = sh(&runtime, AGENT_B, denied).await?;
            assert_ne!(out.code, 0, "B→A must fail: `{denied}`");
        }

        // --- task 3: no infrastructure inside ------------------------------
        for sock in [
            "/run/podman/podman.sock",
            "/run/podman",
            "/run/docker.sock",
            "/var/run/docker.sock",
            "/run/dbus/system_bus_socket",
            "/var/run/dbus/system_bus_socket",
            "/run/dbus",
            "/run/systemd/private",
        ] {
            let out = run(&runtime, AGENT_A, &["ls", sock]).await?;
            assert_ne!(
                out.code, 0,
                "host infrastructure {sock} must not exist inside (SPEC §4)"
            );
        }
        let out = sh(&runtime, AGENT_A, "command -v podman docker nerdctl ctl").await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            "",
            "no container tooling inside the parcel"
        );
        let out = sh(
            &runtime,
            AGENT_A,
            "printenv DOCKER_HOST CONTAINER_HOST DOCKER_SOCKET",
        )
        .await?;
        assert_eq!(out.stdout_lossy().trim(), "", "no host endpoint env inside");

        // --- task 3/4: privilege state of the session process --------------
        let out = sh(
            &runtime,
            AGENT_A,
            "grep -E '^(CapEff|CapPrm|NoNewPrivs|Seccomp):' /proc/self/status",
        )
        .await?;
        let status = out.stdout_lossy();
        assert!(
            status.contains("CapEff:\t0000000000000000"),
            "cap-drop=ALL must leave zero effective caps: {status}"
        );
        assert!(
            status.contains("CapPrm:\t0000000000000000"),
            "no permitted caps either: {status}"
        );
        assert!(
            status.contains("NoNewPrivs:\t1"),
            "no-new-privileges must be active: {status}"
        );

        // --- task 4: the private userns must not contain the daemon user ---
        let daemon_uid = host_uid();
        let out = sh(&runtime, AGENT_A, "cat /proc/self/uid_map").await?;
        let map = out.stdout_lossy();
        let mut covers_slot: Option<(u64, u64)> = None;
        for line in map.lines() {
            let f: Vec<u64> = line
                .split_whitespace()
                .map(|t| t.parse().unwrap_or(u64::MAX))
                .collect();
            assert_eq!(f.len(), 3, "uid_map extent: {line}");
            let (inside, host, len) = (f[0], f[1], f[2]);
            assert_ne!(
                host, 0,
                "rootful identity mapping is forbidden (ADR-0011): {map}"
            );
            if inside <= u64::from(UID_A) && u64::from(UID_A) < inside + len {
                covers_slot = Some((inside, host));
            }
            // The daemon's own host uid must not fall inside the mapping
            // "beyond policy": policy IS the disjoint subuid range.
            assert!(
                u64::from(daemon_uid) < host || u64::from(daemon_uid) >= host + len,
                "daemon uid {daemon_uid} collides with the container userns mapping: {map}"
            );
        }
        let (slot_inside, slot_host) = covers_slot.expect("slot uid is mapped");
        assert_ne!(
            slot_host + u64::from(UID_A) - slot_inside,
            u64::from(daemon_uid),
            "the slot's host uid must not be the daemon user"
        );

        // Host-side: per-slot uids are DISTINCT and never the daemon user;
        // daemon-owned shared dirs stay daemon-owned (the "braces").
        let home_a = owner_of(&tree.parcel_home(UID_A));
        let home_b = owner_of(&tree.parcel_home(UID_B));
        let spool_a = owner_of(&tree.courier_mail_recipient(&sanitize_name("1001").unwrap()));
        let spool_b = owner_of(&tree.courier_mail_recipient(&sanitize_name("1002").unwrap()));
        assert_ne!(
            home_a, daemon_uid,
            "home A is subuid-owned, not daemon-owned"
        );
        assert_ne!(
            home_b, daemon_uid,
            "home B is subuid-owned, not daemon-owned"
        );
        assert_ne!(home_a, home_b, "slots never share a host uid (ADR-0011)");
        assert_eq!(
            home_a, spool_a,
            "slot A's home and spool share its host uid"
        );
        assert_eq!(
            home_b, spool_b,
            "slot B's home and spool share its host uid"
        );
        assert_eq!(
            u64::from(home_a),
            slot_host + u64::from(UID_A) - slot_inside,
            "host owner == uid_map(slot) — the mapping really is the subuid range"
        );
        for shared in [
            tree.commission_git(COMMISSION),
            tree.tavern_gossip(),
            tree.courier_freight(),
            tree.courier_mail(),
            tree.market_pkgcache(),
        ] {
            assert_eq!(
                owner_of(&shared),
                daemon_uid,
                "{} stays daemon-owned",
                shared.display()
            );
        }
        // The mapped UID cannot write the ro host dirs directly either
        // (task 1 "perms braces"): 0755 daemon-owned ⇒ "other" has no w.
        assert_eq!(mode_of(&tree.commission_git(COMMISSION)), 0o755);
        assert_eq!(mode_of(&tree.tavern_gossip()), 0o755);
        assert_eq!(mode_of(&tree.courier_freight()), 0o710);
        assert_eq!(mode_of(&tree.courier_mail()), 0o755);
        assert_eq!(mode_of(&tree.parcel_home(UID_A)), 0o700);
        assert_eq!(
            mode_of(&tree.courier_mail_recipient(&sanitize_name("1001").unwrap())),
            0o700
        );

        runtime
            .stop(AGENT_A, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        runtime
            .stop(AGENT_B, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    cleanup();
    result.expect("guarded escape suite: SPEC §4 isolation invariants");
}

// ---------------------------------------------------------------------------
// 2. Bootstrap integrity: no .env secret material in home, mirror, or image
//    layers (task item 7).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_bootstrap_integrity_no_secrets_in_home_mirror_or_image_layers() {
    if let Some(reason) = podman_guard() {
        eprintln!(
            "SKIP podman_bootstrap_integrity_no_secrets_in_home_mirror_or_image_layers: {reason}"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    fs::write(
        tree.register_brief(&sanitize_name(AGENT_A).unwrap()),
        format!("# {AGENT_A}\n\nQA integrity brief.\n"),
    )
    .expect("brief");
    fs::write(
        tree.env_file(),
        format!("HAMLET_QA_SECRET_KEY={FAKE_SECRET}\n"),
    )
    .expect(".env fixture");

    let config = Config::default();
    let runtime =
        Arc::new(PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3)));
    let provisioner = Provisioner::new(
        tree.clone(),
        config,
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    let cleanup = || {
        let _ = StdCommand::new("podman")
            .args(["rm", "-f", AGENT_A])
            .output();
        reclaim(&tree.parcel_home(UID_A));
        reclaim(&tree.courier_mail_recipient(&sanitize_name("1001").unwrap()));
    };
    cleanup();

    let result = async {
        let provisioned = provisioner
            .provision_parcel_with_report(&sanitize_name(AGENT_A).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        let tag = provisioned.image.tag.clone();

        // --- inside the parcel: home + shared mirror carry no secret -------
        let out = sh(
            &runtime,
            AGENT_A,
            &format!("grep -rIl '{FAKE_SECRET}' /home/{UID_A} /var/pkgcache 2>/dev/null"),
        )
        .await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            "",
            "secret material found in the home or the mirror (SPEC §11.4)"
        );
        let out = sh(
            &runtime,
            AGENT_A,
            &format!("find /home/{UID_A} /var/pkgcache -name '*.env*'"),
        )
        .await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            "",
            "no .env-shaped file in home/mirror"
        );

        // --- image layers: history + config env carry no secret ------------
        let podman_out = |args: &[&str]| -> String {
            let o = StdCommand::new("podman")
                .args(args)
                .output()
                .expect("podman");
            assert!(
                o.status.success(),
                "podman {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        };
        let history = podman_out(&["history", "--no-trunc", "--format", "{{.CreatedBy}}", &tag]);
        assert!(
            !history.contains(FAKE_SECRET),
            "secret baked into an image layer (SPEC §11.4): {history}"
        );
        let inspect = podman_out(&["inspect", "--format", "{{json .Config}}", &tag]);
        assert!(
            !inspect.contains(FAKE_SECRET),
            "secret in image config: {inspect}"
        );
        assert!(
            !inspect.contains(".env"),
            "image config references .env: {inspect}"
        );

        // --- host side: no Containerfile under the village tree ------------
        fn find_containerfile(dir: &Path, hits: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap_or_else(|_| panic!("read {}", dir.display())) {
                let entry = entry.unwrap();
                let path = entry.path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    find_containerfile(&path, hits);
                } else if path
                    .file_name()
                    .map(|n| n == "Containerfile")
                    .unwrap_or(false)
                {
                    hits.push(path.display().to_string());
                }
            }
        }
        let mut hits = Vec::new();
        find_containerfile(tree.root(), &mut hits);
        assert!(
            hits.is_empty(),
            "a Containerfile inside the village tree is root-fs-as-code for \
             any agent (ADR-0012: private 0700 context OUTSIDE the tree): {hits:?}"
        );

        // --- persisted village files carry no secret (charter/DEED/…) ------
        fn scan(dir: &Path, hits: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    scan(&path, hits);
                } else if meta.is_file() && path.file_name().unwrap() != ".env" {
                    if let Ok(text) = fs::read_to_string(&path) {
                        if text.contains(FAKE_SECRET) {
                            hits.push(path.display().to_string());
                        }
                    }
                }
            }
        }
        let mut secret_hits = Vec::new();
        scan(tree.root(), &mut secret_hits);
        assert!(
            secret_hits.is_empty(),
            "secret persisted outside .env: {secret_hits:?} (SPEC §11.4)"
        );

        runtime
            .stop(AGENT_A, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    cleanup();
    result.expect("guarded bootstrap integrity: SPEC §11.4 / ADR-0012");
}
