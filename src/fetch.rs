//! Archives the p0 original of each feed illust into a flat directory.
//!
//! The filename is the URL basename, so every consumer can derive the path
//! from the metadata alone. When an artist edits a work its URL (and hence
//! filename) changes: the new version is downloaded next to the old one and
//! the item is flagged `updated` so hooks re-announce it. Files are only ever
//! added, never deleted — old versions are archival evidence.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use pixiv::download::DownloadClient;
use pixiv::model::{MetaPage, MetaSinglePage};
use rusqlite::Connection;
use serde::Deserialize;

use crate::{Item, store};

// A partial `pixiv::model::Illust`.
#[derive(Deserialize)]
struct Illust {
    page_count: u32,
    meta_single_page: Option<MetaSinglePage>,
    #[serde(default)]
    meta_pages: Vec<MetaPage>,
}

/// The p0 original URL from raw illust JSON, if it has one.
fn p0_url(data: &str) -> Option<String> {
    let illust: Illust = serde_json::from_str(data).ok()?;
    if illust.page_count == 1 {
        illust.meta_single_page?.original_image_url
    } else {
        Some(illust.meta_pages.into_iter().next()?.image_urls.original)
    }
}

fn basename(url: &str) -> Option<&str> {
    url.rsplit('/').next().filter(|s| !s.is_empty())
}

async fn download(fetcher: &DownloadClient, url: &str, path: &Path) -> Result<u64> {
    let mut r = fetcher.download(url).await?;
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp)?;
    let result: Result<u64> = async {
        let mut n = 0u64;
        while let Some(chunk) = r.chunk().await? {
            file.write_all(&chunk)?;
            n += chunk.len() as u64;
        }
        file.sync_all()?;
        Ok(n)
    }
    .await;
    match result {
        Ok(n) => {
            fs::rename(&tmp, path)?;
            Ok(n)
        }
        Err(e) => {
            if let Err(e) = fs::remove_file(&tmp) {
                error!("remove {}: {e}", tmp.display());
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p0_from_single_and_multi_page() {
        let single = r#"{"page_count": 1, "meta_single_page":
            {"original_image_url": "https://i.pximg.net/img-original/img/2026/07/15/22/27/27/147240399_p0.png"},
            "meta_pages": []}"#;
        assert_eq!(
            p0_url(single).as_deref().and_then(basename),
            Some("147240399_p0.png")
        );

        let multi = r#"{"page_count": 30, "meta_single_page": {},
            "meta_pages": [{"image_urls":
            {"original": "https://i.pximg.net/img-original/img/x/147250930-b9e89f03f2dcc5cb214a2a7be5078c58_p0.jpg"}}]}"#;
        assert_eq!(
            p0_url(multi).as_deref().and_then(basename),
            Some("147250930-b9e89f03f2dcc5cb214a2a7be5078c58_p0.jpg")
        );

        assert_eq!(p0_url(r#"{"page_count": 1, "meta_pages": []}"#), None);
    }
}

/// Downloads every item whose current-URL file is missing, and marks items
/// whose p0 URL differs from the archived metadata as `updated`. Must run
/// before `store::archive_illusts` overwrites the old metadata and before
/// hooks fire, so consumers are only told about files that exist.
pub async fn process(fetcher: &DownloadClient, dir: &Path, conn: &Connection, items: &mut [Item]) {
    for item in items.iter_mut() {
        let Some(url) = p0_url(item.data.get()) else {
            continue;
        };
        let Some(name) = basename(&url) else {
            continue;
        };

        match store::get_illust_data(conn, item.iid) {
            Ok(Some(old)) => {
                if p0_url(&old).as_deref().and_then(basename) != Some(name) {
                    item.updated = true;
                }
            }
            Ok(None) => {}
            Err(e) => error!("fetch: read archived {}: {e:#?}", item.iid),
        }

        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        match download(fetcher, &url, &path).await {
            Ok(n) => {
                info!("fetched {name} ({n} bytes)");
                // A recovered missing file is news to consumers too.
                item.updated = true;
            }
            // The next poll retries.
            Err(e) => error!("fetch {name}: {e:#?}"),
        }
    }
}
