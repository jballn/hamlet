//! `hamlet vault` client commands over local API (SPEC §7, §9.4, §12.1; ADR-0038, ADR-0041, ADR-0042).

use std::path::Path;

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;

use crate::client::{ClientError, HamletClient};

/// Execute `hamlet vault list`.
pub async fn list(
    dir_flag: Option<&Path>,
    commission: Option<&str>,
    json: bool,
) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;

    let path = match commission {
        Some(slug) => {
            let safe_slug = sanitize_name(slug).map_err(|e| {
                ClientError::Operational(format!("invalid commission slug '{slug}': {e}"))
            })?;
            format!("{ENDPOINT_VAULT}?commission={}", safe_slug.as_str())
        }
        None => ENDPOINT_VAULT.to_string(),
    };

    let items: Vec<VaultItemView> = client.get(&path).await?;

    if json {
        return serde_json::to_string_pretty(&items)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let headers = ["COMMISSION", "ARTIFACT", "SIZE", "SHA256"];
    if items.is_empty() {
        return Ok(format!(
            "{}  {}  {}  {}\n(no vault artifacts)\n",
            headers[0], headers[1], headers[2], headers[3]
        ));
    }

    let rows: Vec<[String; 4]> = items
        .iter()
        .map(|item| {
            [
                item.commission
                    .as_ref()
                    .map_or("-".to_string(), |c| c.to_string()),
                item.name.clone(),
                item.size_bytes.to_string(),
                item.sha256.clone(),
            ]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
    ];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
    );

    for row in &rows {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        ));
    }

    Ok(out)
}

/// Execute `hamlet vault export`.
pub async fn export(
    dir_flag: Option<&Path>,
    artifact: &str,
    dest: &Path,
    commission: Option<&str>,
    json: bool,
) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;

    let safe_commission = match commission {
        Some(s) => Some(sanitize_name(s).map_err(|e| {
            ClientError::Operational(format!("invalid commission slug '{s}': {e}"))
        })?),
        None => None,
    };

    let req = VaultExportRequest {
        artifact: artifact.to_string(),
        destination: dest.display().to_string(),
        commission: safe_commission.clone(),
    };

    let resp: VaultExportResponse = client.post(ENDPOINT_VAULT_EXPORT, &req).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let comm_str = resp
        .commission
        .as_ref()
        .or(safe_commission.as_ref())
        .map_or("-", |c| c.as_str());
    Ok(format!(
        "exported {} from commission '{}' to {} ({} bytes, sha256: {})\n",
        resp.artifact, comm_str, resp.destination, resp.bytes_written, resp.sha256
    ))
}
