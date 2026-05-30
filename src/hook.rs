use anyhow::Result;
use log::debug;

pub async fn send_illusts<T>(
    url: &str,
    illusts: &[(T, Box<serde_json::value::RawValue>)],
    status: &str,
) -> Result<()> {
    debug!("hook: sending {} new illusts to {url}", illusts.len());

    let mut payloads = Vec::with_capacity(std::cmp::min(illusts.len(), 10));
    for (_, raw) in illusts.iter().take(10).rev() {
        let mut value: serde_json::Value = serde_json::from_str(raw.get())?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "status".to_string(),
                serde_json::Value::String(status.to_string()),
            );
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
