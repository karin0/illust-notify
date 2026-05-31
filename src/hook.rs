use crate::{App, Item};
use anyhow::Result;
use log::{debug, error};
use serde_json::{Value, json};

pub async fn send_illusts(url: &str, illusts: &[Item], app: &App) -> Result<()> {
    let n = illusts.len();
    let illusts: Vec<_> = illusts
        .iter()
        .filter(|item| item.new)
        .map(|item| item.data.get())
        .collect();
    debug!(
        "hook: sending {} new illusts (out of {n}) to {url}",
        illusts.len()
    );
    if illusts.is_empty() {
        return Ok(());
    }

    let status = json!({
        "dist": app.dist,
        "iid": app.iid,
        "since": app.since.unix_timestamp(),
        "since_ago": app.since_ago(),
        "remain": app.remain,
        "skip": app.skip,
    });

    let mut payloads = Vec::with_capacity(std::cmp::min(illusts.len(), 10));
    for data in illusts.into_iter().take(10).rev() {
        let mut value: Value = serde_json::from_str(data)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("_illust_notify".into(), status.clone());
        } else {
            error!("hook: bad value: {data}");
        }
        payloads.push(value);
    }

    let body_str = serde_json::to_string(&payloads)?;
    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body_str)
        .send()
        .await?;

    let status_code = response.status();
    if !status_code.is_success() {
        let body = response.text().await?;
        anyhow::bail!("Webhook POST failed (status: {status_code}): {body}");
    }

    debug!("hook: pushed {} illusts, {status_code}", payloads.len());
    Ok(())
}
