//! `hamlet agent` client commands over local API (SPEC §10, §12.1; ADR-0041, ADR-0042).

use std::path::Path;

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;

use crate::client::{ClientError, HamletClient};

/// Execute `hamlet agent list`.
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
            format!("{ENDPOINT_CENSUS}?commission={}", safe_slug.as_str())
        }
        None => ENDPOINT_CENSUS.to_string(),
    };

    let items: Vec<CensusRecordView> = client.get(&path).await?;

    if json {
        return serde_json::to_string_pretty(&items)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let headers = [
        "UID",
        "IDENTITY",
        "COMMISSION",
        "STATE",
        "SESSIONS",
        "TRACES",
        "LAST_EVENT",
    ];

    if items.is_empty() {
        return Ok(format!(
            "{}  {}  {}  {}  {}  {}  {}\n(no agents)\n",
            headers[0], headers[1], headers[2], headers[3], headers[4], headers[5], headers[6]
        ));
    }

    let rows: Vec<[String; 7]> = items
        .iter()
        .map(|item| {
            let comm = item.commission_slug.as_ref().map_or_else(
                || item.commission.map_or("-".to_string(), |c| c.to_string()),
                |s| s.to_string(),
            );
            let state = if item.stasis {
                "stasis".to_string()
            } else {
                item.state.clone()
            };
            let last_ev = item.last_event.as_deref().unwrap_or("-").to_string();

            [
                item.uid.to_string(),
                item.identity.to_string(),
                comm,
                state,
                item.session_count.to_string(),
                item.counters.traces.to_string(),
                last_ev,
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
    ];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        headers[6],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
        w6 = widths[6],
    );

    for row in &rows {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            row[6],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
            w6 = widths[6],
        ));
    }

    Ok(out)
}

/// Execute `hamlet agent show`.
pub async fn show(dir_flag: Option<&Path>, uid: u32, json: bool) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let items: Vec<CensusRecordView> = client.get(ENDPOINT_CENSUS).await?;

    let item = items
        .into_iter()
        .find(|i| i.uid == uid)
        .ok_or_else(|| ClientError::Operational(format!("agent slot {uid} not found in census")))?;

    if json {
        return serde_json::to_string_pretty(&item)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    let comm = item.commission_slug.as_ref().map_or_else(
        || item.commission.map_or("-".to_string(), |c| c.to_string()),
        |s| s.to_string(),
    );

    let out = format!(
        "agent: parcel{} ({})\n\
         \x20 uid:           {}\n\
         \x20 identity:      {}\n\
         \x20 commission:    {}\n\
         \x20 state:         {}\n\
         \x20 stasis:        {}\n\
         \x20 sessions:      {}\n\
         \x20 traces:        {}\n\
         \x20 input tokens:  {}\n\
         \x20 output tokens: {}\n\
         \x20 cost:          ${:.4}\n\
         \x20 last event:    {}\n",
        item.uid,
        item.identity,
        item.uid,
        item.identity,
        comm,
        item.state,
        if item.stasis { "yes" } else { "no" },
        item.session_count,
        item.counters.traces,
        item.counters.input_tokens,
        item.counters.output_tokens,
        item.counters.cost_micro_usd as f64 / 1_000_000.0,
        item.last_event.as_deref().unwrap_or("-")
    );

    Ok(out)
}

/// Execute `hamlet agent hibernate`.
pub async fn hibernate(
    dir_flag: Option<&Path>,
    uid: u32,
    trigger: Option<&str>,
    json: bool,
) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let path = format!("/api/v1/agents/{uid}/hibernate");
    let req = AgentHibernateRequest {
        trigger: trigger.map(String::from),
    };

    let resp: AgentHibernateResponse = client.post(&path, &req).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(format!(
        "hamlet agent hibernate: parcel {} hibernated into patron {}\n",
        resp.uid, resp.patron
    ))
}

/// Execute `hamlet agent wake`.
pub async fn wake(
    dir_flag: Option<&Path>,
    patron: &str,
    commission: Option<&str>,
    json: bool,
) -> Result<String, ClientError> {
    let safe_patron = sanitize_name(patron)
        .map_err(|e| ClientError::Operational(format!("invalid patron name '{patron}': {e}")))?;

    let safe_commission = match commission {
        Some(s) => Some(sanitize_name(s).map_err(|e| {
            ClientError::Operational(format!("invalid commission slug '{s}': {e}"))
        })?),
        None => None,
    };

    let client = HamletClient::discover(dir_flag)?;
    let req = AgentWakeRequest {
        patron: safe_patron,
        commission: safe_commission,
    };

    let resp: AgentWakeResponse = client.post(ENDPOINT_AGENT_WAKE, &req).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(format!(
        "hamlet agent wake: patron {} awakened as slot {}\n",
        patron, resp.allocated_uid
    ))
}

/// Execute `hamlet agent steer`.
pub async fn steer(
    dir_flag: Option<&Path>,
    uid: u32,
    directive: &str,
    json: bool,
) -> Result<String, ClientError> {
    let client = HamletClient::discover(dir_flag)?;
    let path = format!("/api/v1/agents/{uid}/steer");
    let req = AgentSteerRequest {
        directive: directive.to_string(),
    };

    let resp: AgentSteerResponse = client.post(&path, &req).await?;

    if json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|e| ClientError::Operational(format!("JSON serialization error: {e}")));
    }

    Ok(format!(
        "hamlet agent steer: injected directive into agent {} (correlation: {})\n",
        resp.uid, resp.correlation
    ))
}
