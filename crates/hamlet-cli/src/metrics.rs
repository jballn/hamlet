//! `hamlet metrics` client commands over local API (SPEC §11.2, §12.1; ADR-0017, ADR-0041, ADR-0042).

use std::fs;
use std::path::Path;

use hamlet_core::api::*;

use crate::client::{ClientError, HamletClient};

/// Execute `hamlet metrics show`.
pub async fn show(dir_flag: Option<&Path>, _live: bool, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let summary: MetricsSummaryView = client.get(ENDPOINT_METRICS).await?;

    if json {
        return serde_json::to_string_pretty(&summary)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let mut out = format!(
        "Hamlet Village Metrics\n\
         ----------------------\n\
         Total Input Tokens:  {}\n\
         Total Output Tokens: {}\n\
         Total Cost:          ${:.4}\n\
         Active Sessions:     {}\n\n",
        summary.total_input_tokens,
        summary.total_output_tokens,
        summary.total_cost_micro_usd as f64 / 1_000_000.0,
        summary.active_sessions
    );

    let headers = [
        "UID",
        "IDENTITY",
        "COMMISSION",
        "IN_TOKENS",
        "OUT_TOKENS",
        "COST ($)",
        "CRASHES",
        "UPTIME",
    ];

    if summary.agents.is_empty() {
        out.push_str("(no per-agent metrics recorded)\n");
        return Ok(out);
    }

    let rows: Vec<[String; 8]> = summary
        .agents
        .iter()
        .map(|item| {
            [
                item.uid.to_string(),
                item.identity.to_string(),
                item.commission.map_or("-".to_string(), |c| c.to_string()),
                item.input_tokens.to_string(),
                item.output_tokens.to_string(),
                format!("{:.4}", item.cost_micro_usd as f64 / 1_000_000.0),
                item.crashes.to_string(),
                format!("{}s", item.uptime_secs),
            ]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
        headers[5].len(),
        headers[6].len(),
        headers[7].len(),
    ];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    out.push_str(&format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}  {:<w7$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        headers[6],
        headers[7],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
        w6 = widths[6],
        w7 = widths[7],
    ));

    for row in &rows {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}  {:<w7$}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            row[6],
            row[7],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
            w6 = widths[6],
            w7 = widths[7],
        ));
    }

    Ok(out)
}

/// Execute `hamlet metrics export`.
pub async fn export(
    dir_flag: Option<&Path>,
    dest: Option<&Path>,
    json: bool,
) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let summary: MetricsSummaryView = client.get(ENDPOINT_METRICS).await?;

    let json_text = serde_json::to_string_pretty(&summary)
        .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")))?;

    if let Some(dest_path) = dest {
        fs::write(dest_path, &json_text).map_err(|e| {
            ClientError::Operational(format!(
                "failed to write metrics export to {}: {e}",
                dest_path.display()
            ))
        })?;
        if json {
            return Ok(format!("{{\"exported\":\"{}\"}}\n", dest_path.display()));
        }
        return Ok(format!(
            "metrics successfully exported to {}\n",
            dest_path.display()
        ));
    }

    Ok(json_text)
}
