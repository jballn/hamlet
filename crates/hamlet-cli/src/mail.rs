//! `hamlet mail` client commands over local API (SPEC §8.1, §12.1; ADR-0019, ADR-0041, ADR-0042).

use std::path::Path;

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;

use crate::client::{ClientError, HamletClient};

/// Execute `hamlet mail list`.
pub async fn list(dir_flag: Option<&Path>, uid: u32, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let path = format!("/api/v1/mail/{uid}");
    let items: Vec<MailMessageSummary> = client.get(&path).await?;

    if json {
        return serde_json::to_string_pretty(&items)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let headers = ["FILE", "FROM", "TO", "STATUS", "SUBJECT"];
    if items.is_empty() {
        return Ok(format!(
            "{}  {}  {}  {}  {}\n(no mail for slot {})\n",
            headers[0], headers[1], headers[2], headers[3], headers[4], uid
        ));
    }

    let rows: Vec<[String; 5]> = items
        .iter()
        .map(|item| {
            [
                item.file_name.clone(),
                item.from.clone(),
                item.to.to_string(),
                if item.read { "read" } else { "unread" }.to_string(),
                item.subject.clone(),
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

/// Execute `hamlet mail read`.
pub async fn read(
    dir_flag: Option<&Path>,
    uid: u32,
    file: Option<&str>,
    json: bool,
) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;

    match file {
        Some(file_name) => {
            let path = format!("/api/v1/mail/{uid}/{file_name}");
            let detail: MailMessageDetail = client.get(&path).await?;

            if json {
                return serde_json::to_string_pretty(&detail).map_err(|e| {
                    ClientError::Operational(format!("JSON serialization error: {e}"))
                });
            }

            let mut out = format!(
                "From:    {}\n\
                 To:      {}\n\
                 Subject: {}\n\
                 Status:  {}\n\
                 File:    {}\n\n\
                 {}\n",
                detail.summary.from,
                detail.summary.to,
                detail.summary.subject,
                if detail.summary.read {
                    "read"
                } else {
                    "unread"
                },
                detail.summary.file_name,
                detail.body
            );
            if !out.ends_with('\n') {
                out.push('\n');
            }
            Ok(out)
        }
        None => list(dir_flag, uid, json).await,
    }
}

/// Execute `hamlet mail send`.
pub async fn send(
    dir_flag: Option<&Path>,
    from: Option<&str>,
    to: u32,
    subject: &str,
    body: Option<&str>,
    json: bool,
) -> Result<String, ClientError> {
    if let Some(s) = from {
        let _ = sanitize_name(s)
            .map_err(|e| ClientError::Operational(format!("invalid sender name '{s}': {e}")))?;
    }

    let body_text = body.unwrap_or("").to_string();
    let client = HamletClient::discover(dir_flag)?;
    let req = MailSendRequest {
        from: from.map(String::from),
        to,
        subject: subject.to_string(),
        body: body_text,
    };

    let resp: MailSendResponse = client.post(ENDPOINT_MAIL_SEND, &req).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(format!(
        "hamlet mail send: delivered to slot {} (spool: {})\n",
        to, resp.spool_file
    ))
}
