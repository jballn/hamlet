//! `hamlet commission` client commands over local API (SPEC §12.1; ADR-0041, ADR-0042).

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;

use crate::client::{ClientError, HamletClient};

/// Execute `hamlet commission list`.
pub async fn list(dir_flag: Option<&Path>, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let items: Vec<CommissionSummaryView> = client.get(ENDPOINT_COMMISSIONS).await?;

    if json {
        return serde_json::to_string_pretty(&items)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let headers = ["ID", "SLUG", "STATE", "ROSTER", "BRIEF", "INDENTURE", "GIT"];
    if items.is_empty() {
        return Ok(format!(
            "{}  {}  {}  {}  {}  {}  {}\n(no commissions)\n",
            headers[0], headers[1], headers[2], headers[3], headers[4], headers[5], headers[6]
        ));
    }

    // Determine column widths
    let mut w_id = headers[0].len();
    let mut w_slug = headers[1].len();
    let mut w_state = headers[2].len();
    let mut w_roster = headers[3].len();
    let mut w_brief = headers[4].len();
    let mut w_indenture = headers[5].len();
    let mut w_git = headers[6].len();

    let rows: Vec<[String; 7]> = items
        .iter()
        .map(|item| {
            let id = item.commission_id.to_string();
            let slug = item.slug.to_string();
            let state = item.state.clone();
            let roster = if item.roster.is_empty() {
                "-".to_string()
            } else {
                item.roster
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let brief = if item.brief { "yes" } else { "no" }.to_string();
            let indenture = if item.indenture { "yes" } else { "no" }.to_string();
            let git = item.git.clone();

            w_id = w_id.max(id.len());
            w_slug = w_slug.max(slug.len());
            w_state = w_state.max(state.len());
            w_roster = w_roster.max(roster.len());
            w_brief = w_brief.max(brief.len());
            w_indenture = w_indenture.max(indenture.len());
            w_git = w_git.max(git.len());

            [id, slug, state, roster, brief, indenture, git]
        })
        .collect();

    let mut out = format!(
        "{:<w_id$}  {:<w_slug$}  {:<w_state$}  {:<w_roster$}  {:<w_brief$}  {:<w_indenture$}  {}\n",
        headers[0], headers[1], headers[2], headers[3], headers[4], headers[5], headers[6]
    );

    for r in rows {
        out.push_str(&format!(
            "{:<w_id$}  {:<w_slug$}  {:<w_state$}  {:<w_roster$}  {:<w_brief$}  {:<w_indenture$}  {}\n",
            r[0], r[1], r[2], r[3], r[4], r[5], r[6]
        ));
    }

    Ok(out)
}

/// Execute `hamlet commission new`.
pub async fn new(
    dir_flag: Option<&Path>,
    slug: &str,
    brief_arg: &str,
    seed_arg: Option<&Path>,
    json: bool,
) -> Result<String, ClientError> {
    let safe_slug = sanitize_name(slug)
        .map_err(|e| ClientError::Operational(format!("invalid commission slug '{slug}': {e}")))?;

    let brief_text = read_brief_body(brief_arg)?;
    let seed_str = seed_arg.map(|p| p.display().to_string());

    let client = HamletClient::discover(dir_flag)?;
    let req = CommissionNewRequest {
        slug: safe_slug,
        brief: brief_text,
        seed_dir: seed_str,
    };

    let resp: CommissionNewResponse = client.post(ENDPOINT_COMMISSION_NEW, &req).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(resp.message)
}

/// Execute `hamlet commission show`.
pub async fn show(dir_flag: Option<&Path>, slug: &str, json: bool) -> Result<String, ClientError> {
    let safe_slug = sanitize_name(slug)
        .map_err(|e| ClientError::Operational(format!("invalid commission slug '{slug}': {e}")))?;

    let client = HamletClient::discover(dir_flag)?;
    let path = format!("{ENDPOINT_COMMISSIONS}/{}", safe_slug.as_str());
    let detail: CommissionDetailView = client.get(&path).await?;

    if json {
        return serde_json::to_string_pretty(&detail)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let roster_str = if detail.summary.roster.is_empty() {
        "none".to_string()
    } else {
        detail
            .summary
            .roster
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let git_head_str = match (&detail.git_head, &detail.main_oid) {
        (Some(head), Some(_)) => format!("HEAD -> {head}, main: seeded"),
        (Some(head), None) => format!("HEAD -> {head}, main: unborn"),
        (None, _) => "missing".to_string(),
    };

    let brief_status = if detail.brief.is_some() {
        "present"
    } else {
        "absent"
    };

    let indenture_status = if detail.indenture_tasks.is_empty() {
        "absent"
    } else {
        "present"
    };

    let mut out = format!(
        "commission: commission{} ({})\n\
         \x20 id:         {}\n\
         \x20 slug:       {}\n\
         \x20 state:      {}\n\
         \x20 roster:     {}\n\
         \x20 ledger:     guild/ledger/commission{}\n\
         \x20 git:        guild/ledger/commission{}/git ({})\n\
         \x20 brief:      guild/ledger/commission{}/COMMISSION.md ({})\n\
         \x20 indenture:  guild/ledger/commission{}/INDENTURE.md ({})\n",
        detail.summary.commission_id,
        detail.summary.slug,
        detail.summary.commission_id,
        detail.summary.slug,
        detail.summary.state,
        roster_str,
        detail.summary.commission_id,
        detail.summary.commission_id,
        git_head_str,
        detail.summary.commission_id,
        brief_status,
        detail.summary.commission_id,
        indenture_status
    );

    if !detail.indenture_tasks.is_empty() {
        let entries_str = detail
            .indenture_tasks
            .iter()
            .map(|e| match &e.identity {
                Some(ident) => format!("{} ({ident}): {}", e.uid, e.status),
                None => format!("{}: {}", e.uid, e.status),
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  entries:    {entries_str}\n"));
    }

    if let Some(ref brief) = detail.brief {
        out.push_str("\n--- COMMISSION.md ---\n");
        out.push_str(brief);
        if !brief.ends_with('\n') {
            out.push('\n');
        }
    }

    Ok(out)
}

/// Execute `hamlet commission close`.
pub async fn close(dir_flag: Option<&Path>, slug: &str, json: bool) -> Result<String, ClientError> {
    let safe_slug = sanitize_name(slug)
        .map_err(|e| ClientError::Operational(format!("invalid commission slug '{slug}': {e}")))?;

    let client = HamletClient::discover(dir_flag)?;
    let path = format!("/api/v1/commissions/{}/close", safe_slug.as_str());
    let resp: CommissionCloseResponse = client.post_empty(&path).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(format!(
        "hamlet commission close: commission{} ({})\n\
         \x20 state: {}\n\
         \x20 parcels stopped: {}\n\
         \x20 escalation filed: {}\n",
        resp.commission_id, resp.slug, resp.state, resp.parcels_stopped, resp.escalation_filed
    ))
}

fn read_brief_body(brief_arg: &str) -> Result<String, ClientError> {
    if brief_arg == "-" {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).map_err(|e| {
            ClientError::Operational(format!("failed to read brief from stdin: {e}"))
        })?;
        return Ok(buf);
    }

    let path = Path::new(brief_arg);
    let meta = fs::symlink_metadata(path).map_err(|e| {
        ClientError::Operational(format!("failed to read brief file {}: {e}", path.display()))
    })?;

    if meta.file_type().is_symlink() {
        return Err(ClientError::Operational(format!(
            "symlink brief path {} refused (ADR-0007)",
            path.display()
        )));
    }

    if !meta.is_file() {
        return Err(ClientError::Operational(format!(
            "brief path {} is not a regular file",
            path.display()
        )));
    }

    fs::read_to_string(path).map_err(|e| {
        ClientError::Operational(format!("failed to read brief file {}: {e}", path.display()))
    })
}
