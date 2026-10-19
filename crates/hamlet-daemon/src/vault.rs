//! Vault deliverable promotion governor (SPEC §7, §9.4, §11.2; ADR-0038).
//!
//! Evaluates approved / rejected promotion decrees from the Mayor:
//! - On `ApprovePromotion`: verifies freight path exists and is a regular file,
//!   refuses symlinks (ADR-0007), calculates checksum (content_digest),
//!   atomically copies artifact into `keep/vault/<commission-slug>/<artifact>`
//!   (with `.sha256` checksum sidecar), advances commission state to `Closed`
//!   (`PromotionApproved`), marks stasis on submitting parcel, triggers session
//!   graceful stop, and exports telemetry to `keep/archives/`.
//! - On `RejectPromotion`: increments rejection count, delivers line-numbered
//!   rejection report to the agent via mail (`From: system`), traces to sled.
//!   Bounded: up to `max_rejections` (default 3), resets `deliverable_in_review`
//!   so the agent can rework and re-submit; upon exceeding bounds, files an
//!   `EscalationNotice` and transitions commission to `Failed` (`PromotionRejected`).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::charter::Charter;
use hamlet_core::commission::{CommissionEvent, CommissionState};
use hamlet_core::decree::{
    content_digest, DecreeId, EscalationNotice, TreeRelPath, ESCALATION_VERSION,
};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;

use crate::decrees::{DecreeRefusal, Promotion};
use crate::ledger::CommissionWake;
use crate::mailroute::{MailActor, MailRouter};
use crate::supervise::Supervisor;
use crate::telemetry::{AgentKey, Telemetry, TraceRecord};

pub const DEFAULT_MAX_PROMOTION_REJECTIONS: u32 = 3;
const MODE_FILE: u32 = 0o644;

/// Production deliverable promotion governor implementing [`Promotion`].
pub struct VaultPromotion {
    tree: HamletTree,
    telemetry: Option<Arc<Telemetry>>,
    mail: Option<Arc<MailRouter>>,
    supervisor: Option<Arc<Supervisor>>,
    max_rejections: u32,
    rejections: Mutex<BTreeMap<String, u32>>,
    wake: Option<Arc<dyn CommissionWake>>,
}

impl VaultPromotion {
    /// Create a new promotion engine over `tree`.
    #[must_use]
    pub fn new(tree: HamletTree) -> Self {
        Self {
            tree,
            telemetry: None,
            mail: None,
            supervisor: None,
            max_rejections: DEFAULT_MAX_PROMOTION_REJECTIONS,
            rejections: Mutex::new(BTreeMap::new()),
            wake: None,
        }
    }

    /// Attach telemetry for recording sled traces and archive exports.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Attach mail router for delivering rejection notices to agents.
    #[must_use]
    pub fn with_mail(mut self, mail: Arc<MailRouter>) -> Self {
        self.mail = Some(mail);
        self
    }

    /// Attach supervisor for graceful session spin-down.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Arc<Supervisor>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// Set maximum allowed rejections before human escalation (default 3).
    #[must_use]
    pub fn with_max_rejections(mut self, max: u32) -> Self {
        self.max_rejections = max;
        self
    }

    /// Attach wake hook.
    #[must_use]
    pub fn with_wake(mut self, wake: Arc<dyn CommissionWake>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Get current rejection count for a freight path.
    pub fn rejection_count(&self, freight_path: &str) -> u32 {
        self.rejections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(freight_path)
            .copied()
            .unwrap_or(0)
    }

    fn resolve_freight_file(&self, freight_path: &str) -> Result<PathBuf, DecreeRefusal> {
        let cleaned = freight_path.trim();
        let target = if cleaned.starts_with("courier/freight/") {
            self.tree.root().join(cleaned)
        } else if cleaned.starts_with("freight/") {
            self.tree.root().join("courier").join(cleaned)
        } else {
            self.tree.courier_freight().join(cleaned)
        };

        let freight_dir = self.tree.courier_freight();
        let rel = match target.strip_prefix(&freight_dir) {
            Ok(r) => r,
            Err(_) => {
                return Err(DecreeRefusal::invalid_target(format!(
                    "freight path {freight_path} is not under courier/freight"
                )));
            }
        };

        // Check every path component for symlinks (ADR-0007)
        let mut current = freight_dir.clone();
        for comp in rel.components() {
            current.push(comp);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        return Err(DecreeRefusal::invalid_target(format!(
                            "symlink refused at {} (ADR-0007)",
                            current.display()
                        )));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "freight path {freight_path} does not exist"
                    )));
                }
                Err(e) => {
                    return Err(DecreeRefusal::execution_failed(format!(
                        "inspect freight path {freight_path}: {e}"
                    )));
                }
            }
        }

        // Ensure leaf is a regular file
        match fs::symlink_metadata(&target) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "freight path {freight_path} is not a regular file"
                    )));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(DecreeRefusal::invalid_target(format!(
                    "freight path {freight_path} does not exist"
                )));
            }
            Err(e) => {
                return Err(DecreeRefusal::execution_failed(format!(
                    "inspect freight path {freight_path}: {e}"
                )));
            }
        }

        // Confinement to courier/freight directory
        let canon_freight = match freight_dir.canonicalize() {
            Ok(r) => r,
            Err(_) => freight_dir,
        };
        match target.canonicalize() {
            Ok(canon_target) => {
                if !canon_target.starts_with(&canon_freight) {
                    return Err(DecreeRefusal::invalid_target(format!(
                        "freight path {freight_path} escapes freight directory"
                    )));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(DecreeRefusal::invalid_target(format!(
                    "freight path {freight_path} does not exist"
                )));
            }
            Err(e) => {
                return Err(DecreeRefusal::execution_failed(format!(
                    "canonicalize freight path {freight_path}: {e}"
                )));
            }
        }

        Ok(target)
    }

    fn identify_commission(
        &self,
        freight_path: &str,
        charter: &Charter,
    ) -> Result<(u64, SafeName, Option<u32>), DecreeRefusal> {
        // 1. Identify sandbox owner or commission from path components first
        for part in freight_path.split('/') {
            if part.is_empty() || part == "courier" || part == "freight" {
                continue;
            }
            // Check if part matches a commission slug directly
            if let Some((&id, comm)) = charter
                .commissions
                .iter()
                .find(|(_, c)| c.slug.as_str() == part)
            {
                let submitting_uid = comm.roster.iter().next().copied();
                return Ok((id, comm.slug.clone(), submitting_uid));
            }

            // Check if part matches a slot identity
            if let Ok(safe_part) = sanitize_name(part) {
                if let Some((&slot_uid, _)) =
                    charter.slots.iter().find(|(_, s)| s.identity == safe_part)
                {
                    if let Some((&id, comm)) = charter
                        .commissions
                        .iter()
                        .find(|(_, c)| c.roster.contains(&slot_uid))
                    {
                        return Ok((id, comm.slug.clone(), Some(slot_uid)));
                    }
                }
            }
            // Check if part matches a slot UID
            if let Ok(uid) = part.parse::<u32>() {
                if charter.slots.contains_key(&uid) {
                    if let Some((&id, comm)) = charter
                        .commissions
                        .iter()
                        .find(|(_, c)| c.roster.contains(&uid))
                    {
                        return Ok((id, comm.slug.clone(), Some(uid)));
                    }
                }
            }
            // Check if part matches "commission<id>" or "<id>"
            let id_str = part.strip_prefix("commission").unwrap_or(part);
            if let Ok(comm_id) = id_str.parse::<u64>() {
                if let Some(comm) = charter.commissions.get(&comm_id) {
                    let submitting_uid = comm.roster.iter().next().copied();
                    return Ok((comm_id, comm.slug.clone(), submitting_uid));
                }
            }
        }

        // 2. If path did not identify a commission/slot, check commissions in review
        let in_review: Vec<_> = charter
            .commissions
            .iter()
            .filter(|(_, c)| c.deliverable_in_review)
            .collect();
        if in_review.len() == 1 {
            let (&id, comm) = in_review[0];
            let submitting_uid = comm.roster.iter().next().copied();
            return Ok((id, comm.slug.clone(), submitting_uid));
        } else if in_review.len() > 1 {
            return Err(DecreeRefusal::invalid_target(format!(
                "freight path {freight_path} is ambiguous: multiple commissions have deliverables in review"
            )));
        }

        // 3. Fallback if single commission exists
        if charter.commissions.len() == 1 {
            let (&id, comm) = charter.commissions.iter().next().unwrap();
            let submitting_uid = comm.roster.iter().next().copied();
            return Ok((id, comm.slug.clone(), submitting_uid));
        }

        // 4. Fallback to active or winding down commission if exactly one exists
        let active_or_winding: Vec<_> = charter
            .commissions
            .iter()
            .filter(|(_, c)| {
                c.state == CommissionState::WindingDown || c.state == CommissionState::Active
            })
            .collect();
        if active_or_winding.len() == 1 {
            let (&id, comm) = active_or_winding[0];
            let submitting_uid = comm.roster.iter().next().copied();
            return Ok((id, comm.slug.clone(), submitting_uid));
        }

        Err(DecreeRefusal::invalid_target(format!(
            "could not associate freight path {freight_path} with any commission"
        )))
    }
}

impl Promotion for VaultPromotion {
    fn approve(&self, freight_path: &str, artifact_name: &SafeName) -> Result<(), DecreeRefusal> {
        let source_path = self.resolve_freight_file(freight_path)?;
        let bytes = fs::read(&source_path).map_err(|e| {
            DecreeRefusal::execution_failed(format!(
                "read freight file {}: {e}",
                source_path.display()
            ))
        })?;
        let checksum = content_digest(&bytes);

        let mut charter = Charter::load(&self.tree.charter())
            .map_err(|e| DecreeRefusal::execution_failed(format!("load charter: {e}")))?;

        let (commission_id, commission_slug, submitting_uid) =
            self.identify_commission(freight_path, &charter)?;

        // Target vault paths
        let vault_dir = self.tree.keep_vault_commission(&commission_slug);
        fs::create_dir_all(&vault_dir).map_err(|e| {
            DecreeRefusal::execution_failed(format!(
                "create vault dir {}: {e}",
                vault_dir.display()
            ))
        })?;

        let dest_path = vault_dir.join(artifact_name.as_str());
        if let Ok(meta) = fs::symlink_metadata(&dest_path) {
            if meta.file_type().is_symlink() {
                return Err(DecreeRefusal::invalid_target(format!(
                    "symlink refused at {} (ADR-0007)",
                    dest_path.display()
                )));
            }
        }

        // Atomic write of the artifact
        write_atomic_file(&dest_path, &bytes, MODE_FILE).map_err(|e| {
            DecreeRefusal::execution_failed(format!(
                "write vault artifact {}: {e}",
                dest_path.display()
            ))
        })?;

        // Re-read written artifact from disk and verify checksum
        let written_bytes = fs::read(&dest_path).map_err(|e| {
            DecreeRefusal::execution_failed(format!(
                "re-read vault artifact {}: {e}",
                dest_path.display()
            ))
        })?;
        let written_checksum = content_digest(&written_bytes);
        if written_checksum != checksum {
            return Err(DecreeRefusal::execution_failed(format!(
                "checksum mismatch for vault artifact {}: expected {checksum}, got {written_checksum}",
                dest_path.display()
            )));
        }

        // Atomic write of checksum sidecar (mandatory)
        let sidecar_path = vault_dir.join(format!("{}.sha256", artifact_name.as_str()));
        let sidecar_content = format!("{checksum}  {}\n", artifact_name.as_str());
        write_atomic_file(&sidecar_path, sidecar_content.as_bytes(), MODE_FILE).map_err(|e| {
            DecreeRefusal::execution_failed(format!(
                "write checksum sidecar {}: {e}",
                sidecar_path.display()
            ))
        })?;

        // Update charter state
        if let Some(comm) = charter.commissions.get_mut(&commission_id) {
            if !comm.deliverable_in_review {
                comm.deliverable_in_review = true;
            }
            comm.apply_event(&CommissionEvent::PromotionApproved)
                .map_err(|e| DecreeRefusal::execution_failed(format!("advance commission: {e}")))?;
        }

        // Set submitting parcel in stasis
        if let Some(uid) = submitting_uid {
            if let Some(lease) = charter.parcels.get_mut(&uid) {
                lease.stasis = true;
            }
        }

        charter
            .save_atomic(&self.tree.charter())
            .map_err(|e| DecreeRefusal::execution_failed(format!("save charter: {e}")))?;

        // Spin down session
        if let Some(uid) = submitting_uid {
            if let Some(sup) = &self.supervisor {
                let sup = Arc::clone(sup);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = sup.stop_session(uid).await;
                    });
                }
            }
        }

        // Export telemetry to archives
        if let Some(tel) = &self.telemetry {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let _ = tel.export_commission_summary(&self.tree, commission_id, now_ms);
            let _ = tel.trace_event(
                AgentKey {
                    commission: commission_id,
                    uid: submitting_uid.unwrap_or(0),
                },
                TraceRecord {
                    at_unix_ms: now_ms,
                    event: "vault_promoted".into(),
                    detail: format!("{artifact_name} digest {checksum}"),
                },
            );
        }

        Ok(())
    }

    fn reject(&self, freight_path: &str, reason: &str) -> Result<(), DecreeRefusal> {
        let count = {
            let mut map = self.rejections.lock().unwrap_or_else(|e| e.into_inner());
            let entry = map.entry(freight_path.to_owned()).or_insert(0);
            *entry += 1;
            *entry
        };

        let mut charter = Charter::load(&self.tree.charter())
            .map_err(|e| DecreeRefusal::execution_failed(format!("load charter: {e}")))?;

        let (commission_id, commission_slug, submitting_uid) =
            self.identify_commission(freight_path, &charter)?;

        // Deliver rejection mail to agent
        if let (Some(mail), Some(uid)) = (&self.mail, submitting_uid) {
            let subject =
                format!("commission:{commission_id}:deliverable:rejected ({commission_slug})");
            let body = format!(
                "Deliverable promotion rejected: {reason}\n\n\
                 Freight path: {freight_path}\n\
                 Attempt: {count}/{}\n",
                self.max_rejections
            );
            let _ = mail.send(MailActor::System, uid, &subject, &body, None);
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Telemetry trace
        if let Some(tel) = &self.telemetry {
            let _ = tel.trace_event(
                AgentKey {
                    commission: commission_id,
                    uid: submitting_uid.unwrap_or(0),
                },
                TraceRecord {
                    at_unix_ms: now_ms,
                    event: "promotion_rejected".into(),
                    detail: format!("{freight_path} attempt {count} reason {reason}"),
                },
            );
        }

        // Bounded check
        if count >= self.max_rejections {
            // Escalate to human
            let notice = EscalationNotice {
                escalation_version: ESCALATION_VERSION,
                decree_id: DecreeId::new(1, "0000000000000001").expect("valid decree id"),
                evidence_refs: vec![TreeRelPath::parse(freight_path.trim_start_matches('/'))
                    .unwrap_or_else(|_| {
                        TreeRelPath::parse("courier/freight").expect("valid rel path")
                    })],
                recorded_unix_ms: now_ms,
                summary: format!("promotion rejected {count} times: {reason}"),
            };

            let escalations_dir = self.tree.manor_escalations();
            let _ = fs::create_dir_all(&escalations_dir);
            let notice_path =
                escalations_dir.join(format!("escalation-commission{commission_id}.json"));
            if let Ok(rendered) = notice.render() {
                let _ = write_atomic_file(&notice_path, rendered.as_bytes(), MODE_FILE);
            }

            // Transition commission to Failed
            if let Some(comm) = charter.commissions.get_mut(&commission_id) {
                if !comm.deliverable_in_review {
                    comm.deliverable_in_review = true;
                }
                let _ = comm.apply_event(&CommissionEvent::PromotionRejected);
            }
            charter
                .save_atomic(&self.tree.charter())
                .map_err(|e| DecreeRefusal::execution_failed(format!("save charter: {e}")))?;
        } else {
            // Rework loop: clear deliverable_in_review so agent can rework and re-submit
            if let Some(comm) = charter.commissions.get_mut(&commission_id) {
                comm.deliverable_in_review = false;
            }
            charter
                .save_atomic(&self.tree.charter())
                .map_err(|e| DecreeRefusal::execution_failed(format!("save charter: {e}")))?;
        }

        Ok(())
    }
}

const TEMP_ATTEMPTS: u32 = 256;

/// Atomically write `bytes` to `path` using tempfile + sync + rename (ADR-0007 create_new discipline).
fn write_atomic_file(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;

    for attempt in 0..TEMP_ATTEMPTS {
        let tmp_name = if attempt == 0 {
            format!(".{file_name}.tmp-{}", std::process::id())
        } else {
            format!(".{file_name}.tmp-{}-{}", std::process::id(), attempt)
        };
        let tmp_path = parent.join(tmp_name);

        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };

        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);

        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
        return fs::rename(&tmp_path, path);
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "no free temp name for {} after {TEMP_ATTEMPTS} attempts (all pre-planted)",
            path.display()
        ),
    ))
}
