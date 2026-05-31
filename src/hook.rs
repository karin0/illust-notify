use crate::{App, Item};
use anyhow::Result;
use log::{debug, error};
use reqwest::{Client, header::CONTENT_TYPE};
use serde_json::json;

const META_FIELD: &[u8] = b",\"_illust_notify\":";

pub async fn send_illusts(http: &Client, url: &str, illusts: &[Item], app: &App) -> Result<()> {
    let n = illusts.len();
    let illusts: Vec<_> = illusts
        .iter()
        .filter(|item| item.new || item.updated)
        .collect();
    debug!(
        "hook: got {} new/updated illusts (out of {n})",
        illusts.len()
    );
    if illusts.is_empty() {
        return Ok(());
    }

    let status = serde_json::to_vec(&json!({
        "dist": app.dist,
        "iid": app.iid,
        "since": app.since.unix_timestamp(),
        "since_ago": app.since_ago(),
        "remain": app.remain,
        "skip": app.skip,
    }))?;

    let illusts = illusts
        .into_iter()
        .rev()
        .map(|item| item.data.get().trim().as_bytes());

    let n = illusts.len();
    // fields: n * inc, commas: n-1, brackets: 2
    let cap = illusts.clone().map(<[u8]>::len).sum::<usize>()
        + n * (META_FIELD.len() + status.len() + 1)
        + 1;
    debug!("hook: sending {cap} bytes of {n} illusts to {url}");

    let mut body = Vec::with_capacity(cap);
    body.push(b'[');
    for data in illusts {
        let len = data.len();
        if data.starts_with(b"{") && data.ends_with(b"}") && len > 2 {
            if body.len() > 1 {
                body.push(b',');
            }
            body.extend_from_slice(&data[..len - 1]);
            body.extend_from_slice(META_FIELD);
            body.extend_from_slice(&status);
            body.push(b'}');
        } else {
            error!("hook: bad illust: {}", String::from_utf8_lossy(data));
        }
    }
    body.push(b']');
    if body.len() != cap {
        error!("hook: expected body length {cap}, got {}", body.len());
    }

    let resp = http
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await?;
    crate::finalize_http(resp).await;
    Ok(())
}
