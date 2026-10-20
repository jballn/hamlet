//! `hamlet decree` client commands over local API (SPEC §7, §12.1; ADR-0026, ADR-0041, ADR-0042).

use std::path::Path;

use hamlet_core::api::*;

use crate::client::{ClientError, HamletClient};

/// Execute `hamlet decree list`.
pub async fn list(dir_flag: Option<&Path>, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let items: Vec<DecreeSummaryView> = client.get(ENDPOINT_DECREES).await?;

    if json {
        return serde_json::to_string_pretty(&items)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let headers = ["DECREE_ID", "SEQ", "ACTION", "STATUS", "REASON"];
    if items.is_empty() {
        return Ok(format!(
            "{}  {}  {}  {}  {}\n(no decrees recorded)\n",
            headers[0], headers[1], headers[2], headers[3], headers[4]
        ));
    }

    let rows: Vec<[String; 5]> = items
        .iter()
        .map(|item| {
            [
                item.decree_id.clone(),
                item.seq.to_string(),
                item.action.clone(),
                item.status.clone(),
                item.reason.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
    ];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
    );

    for row in &rows {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
        ));
    }

    Ok(out)
}

/// Execute `hamlet decree show`.
pub async fn show(dir_flag: Option<&Path>, id: &str, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let path = format!("{ENDPOINT_DECREES}/{id}");
    let detail: DecreeDetailView = client.get(&path).await?;

    if json {
        return serde_json::to_string_pretty(&detail)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let mut out = format!(
        "decree: {}\n\
         \x20 seq:     {}\n\
         \x20 action:  {}\n\
         \x20 status:  {}\n\
         \x20 reason:  {}\n\
         \x20 ack:     {}\n",
        detail.summary.decree_id,
        detail.summary.seq,
        detail.summary.action,
        detail.summary.status,
        detail.summary.reason.as_deref().unwrap_or("-"),
        detail.ack_verdict.as_deref().unwrap_or("none")
    );

    if let Some(ref reason) = detail.ack_reason {
        out.push_str(&format!("  ack reason: {reason}\n"));
    }

    out.push_str("\n--- Payload JSON ---\n");
    out.push_str(&detail.raw_json);
    if !detail.raw_json.ends_with('\n') {
        out.push('\n');
    }

    Ok(out)
}
