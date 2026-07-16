#[macro_use]
extern crate log;

mod fetch;
mod store;

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{env, fs};

use anyhow::{Context as _, Result};
use pixiv::aapi::Restrict;
use pixiv::client::{AuthedClient, AuthedState};
use pixiv::model::IllustId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, format_description, macros::format_description};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};

use pixiv::download::DownloadClient;

#[cfg(feature = "tray")]
use pyo3::prelude::*;

#[cfg(all(feature = "inotify", feature = "notify"))]
compile_error!("feature \"inotify\" and \"notify\" conflict");

#[cfg(feature = "inotify")]
mod inotify;

#[cfg(feature = "notify")]
mod notify;

#[cfg(feature = "tray")]
mod tray;

pub(crate) mod hook;

fn default_delay() -> u32 {
    300
}

fn default_max_pages() -> u32 {
    5
}

fn default_min_skip_pages() -> u32 {
    3
}

const CONFIG_FILE: &str = "config.json";
const DB_FILE: &str = "state.db";

#[cfg(feature = "callback")]
const CALLBACK_FILE: &str = "./callback";

#[cfg(feature = "download")]
const IMG_FILE: &str = "img.jpg";

#[cfg(any(feature = "inotify", feature = "notify"))]
pub(crate) const NOTIFY_FILE: &str = "notify";

#[derive(Deserialize, Debug, Clone)]
struct Config {
    refresh_token: Option<String>,
    #[serde(default = "default_delay")]
    delay: u32,
    #[serde(default = "default_max_pages")]
    max_pages: u32,
    #[serde(default = "default_min_skip_pages")]
    min_skip_pages: u32,
    /// Whether to keep raw metadata of seen illusts in the database.
    #[serde(default)]
    archive: bool,
    /// Consumers of new/updated illusts; empty disables webhooks.
    #[serde(default)]
    hooks: Vec<String>,
    /// Flat directory for archived p0 originals, named by URL basename.
    pix_dir: Option<String>,
    #[cfg(feature = "request")]
    notify_url: Option<String>,
    #[cfg(feature = "tray")]
    python_site_packages: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct ImageUrls {
    square_medium: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Illust {
    id: IllustId,
    title: String,
    create_date: String,
    is_bookmarked: bool,
    image_urls: ImageUrls,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Page {
    illusts: Vec<Box<serde_json::value::RawValue>>,
    next_url: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct AppState {
    iid: IllustId,
    since: OffsetDateTime,
    remain: bool,
    skip: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            iid: 0,
            since: OffsetDateTime::UNIX_EPOCH,
            remain: false,
            skip: false,
        }
    }
}

struct App {
    api: AuthedClient,
    state: AppState,
    #[cfg(feature = "download")]
    downloader: DownloadClient,
    tz: UtcOffset,
    ago: timeago::Formatter,
    db: Connection,
    dist: usize,
}

impl Deref for App {
    type Target = AppState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

const DATE_FORMAT: &[format_description::FormatItem<'static>] =
    format_description!("[month padding:none]/[day padding:none] [hour padding:none]:[minute]");

#[derive(Debug)]
struct Item {
    iid: IllustId,
    data: Box<serde_json::value::RawValue>,
    new: bool,
    /// The p0 URL changed, or its missing file was just fetched (see fetch.rs).
    updated: bool,
}

impl App {
    async fn new(refresh_token: &str, db: Connection) -> Result<Self> {
        Ok(Self {
            api: AuthedClient::new(refresh_token).await?,
            state: AppState::default(),
            #[cfg(feature = "download")]
            downloader: DownloadClient::new(),
            tz: UtcOffset::current_local_offset()?,
            ago: timeago::Formatter::new(),
            dist: store::get_seen_count(&db)?,
            db,
        })
    }

    fn load(state: AppState, api_state: AuthedState, db: Connection) -> Result<Self> {
        Ok(Self {
            api: AuthedClient::load(api_state),
            state,
            #[cfg(feature = "download")]
            downloader: DownloadClient::new(),
            tz: UtcOffset::current_local_offset()?,
            ago: timeago::Formatter::new(),
            dist: store::get_seen_count(&db)?,
            db,
        })
    }

    fn convert_date(&self, date: &str) -> Result<OffsetDateTime> {
        let t = OffsetDateTime::parse(date, &format_description::well_known::Iso8601::DEFAULT)?
            .to_offset(self.tz);
        Ok(t)
    }

    fn since(&self) -> String {
        match self.since.format(&DATE_FORMAT) {
            Ok(s) => s,
            Err(e) => format!("Error: {e:?}"),
        }
    }

    fn since_ago(&self) -> String {
        let now = OffsetDateTime::now_utc().to_offset(self.tz);
        let d = now - self.since;
        self.ago.convert(d.unsigned_abs())
    }

    fn ids(&self, illusts: &[Item]) -> impl Iterator<Item = IllustId> {
        illusts
            .iter()
            .map(|item| item.iid)
            .take_while(|&id| id != self.iid)
    }

    fn extend_seen(&mut self, illusts: &[Item]) -> Result<()> {
        self.dist += store::extend_seen(&self.db, self.ids(illusts))?;
        Ok(())
    }

    fn reset_seen(&mut self, illusts: &[Item]) -> Result<()> {
        self.dist = store::reset_seen(&self.db, self.ids(illusts))?;
        Ok(())
    }

    async fn ensure_authed(&mut self) -> Result<()> {
        if self.api.ensure_authed().await? {
            store::save_token(&self.db, &self.api.state)?;
        }
        Ok(())
    }

    async fn refresh(&mut self, config: &Config) -> Result<Vec<Item>> {
        self.ensure_authed().await?;
        let mut r: Page = self.api.illust_follow(Restrict::Public).await?;

        let mut pn = 1;
        let mut result = Vec::with_capacity(30 * config.min_skip_pages as usize);

        loop {
            debug!("page {} has {} illusts", pn, r.illusts.len());
            let mut may_skip = pn >= config.min_skip_pages;
            let mut done = false;
            for data in r.illusts {
                if done {
                    #[derive(Deserialize)]
                    struct Id {
                        id: IllustId,
                    }
                    let id: Id = serde_json::from_str(data.get())?;
                    result.push(Item {
                        iid: id.id,
                        data,
                        new: false,
                        updated: false,
                    });
                    continue;
                }

                let illust: Illust = serde_json::from_str(data.get())?;
                if illust.is_bookmarked {
                    debug!("bookmarked: {illust:#?}");
                    if self.iid != illust.id {
                        debug!("new id: {} time: {}", illust.id, illust.create_date);
                        self.since = self.convert_date(&illust.create_date)?;

                        #[cfg(feature = "download")]
                        {
                            use std::io::{Seek, Write};

                            let mut image = self
                                .downloader
                                .download(&illust.image_urls.square_medium)
                                .await?;
                            let mut file = fs::File::create(IMG_FILE)?;

                            while let Some(chunk) = image.chunk().await? {
                                file.write_all(&chunk)?;
                            }
                            debug!("downloaded {} bytes", file.stream_position()?);
                        }

                        self.iid = illust.id;
                        store::save_state(&self.db, &self.state)?;
                    }
                    done = true;
                    result.push(Item {
                        iid: illust.id,
                        data,
                        new: false,
                        updated: false,
                    });
                    continue;
                }

                let new = !store::is_seen(&self.db, illust.id)?;
                result.push(Item {
                    iid: illust.id,
                    data,
                    new,
                    updated: false,
                });
                if new {
                    may_skip = false;
                }
            }

            if done {
                self.remain = false;
                self.skip = false;
                self.reset_seen(&result)?;
            } else if may_skip {
                if !self.skip {
                    warn!("skipping from page {pn}");
                    self.skip = true;
                }
            } else if let Some(url) = r.next_url {
                if pn >= config.max_pages {
                    if !self.remain {
                        warn!("reached max pages {pn}");
                        self.remain = true;
                    }
                } else {
                    self.ensure_authed().await?;
                    r = self.api.call_url(&url).await?;
                    pn += 1;
                    continue;
                }
            } else {
                warn!("no more pages");
                self.remain = false;
                self.skip = false;
            }
            self.extend_seen(&result)?;
            return Ok(result);
        }
    }

    fn token(&self) -> (IllustId, usize) {
        (self.iid, self.dist)
    }
}

#[cfg(feature = "callback")]
fn do_callback(bin: &str, args: &[&str]) -> Result<()> {
    debug!("callback: {bin} {args:?}");
    let r = std::process::Command::new(bin).args(args).spawn()?.wait()?;
    if r.success() {
        debug!("callback: {:?}", r.code());
    } else {
        anyhow::bail!("callback returned {:?}", r.code());
    }
    Ok(())
}

#[cfg(feature = "callback")]
struct Callback {
    itoa: itoa::Buffer,
    itoa2: itoa::Buffer,
}

#[cfg(feature = "callback")]
impl Callback {
    fn new() -> Option<Self> {
        if let Ok(metadata) = fs::metadata(CALLBACK_FILE)
            && metadata.is_file()
            && {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                true
            }
        {
            return Some(Self {
                itoa: itoa::Buffer::new(),
                itoa2: itoa::Buffer::new(),
            });
        }
        None
    }
}

fn refresh_token(config: &Config) -> Result<String> {
    env::var("PIXIV_REFRESH_TOKEN")
        .ok()
        .or_else(|| config.refresh_token.clone())
        .or_else(|| option_env!("PIXIV_REFRESH_TOKEN").map(str::to_owned))
        .context("no refresh token: set PIXIV_REFRESH_TOKEN or refresh_token in config.json")
}

async fn finalize_http(resp: reqwest::Response) {
    let st = resp.status();
    debug!("http status: {st}");
    if !st.is_success() {
        match resp.text().await {
            Ok(body) => error!("http failed: {st}: {body}"),
            Err(e) => error!("http failed: {st}, text: {e:#?}"),
        }
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let mut args = env::args();
    args.next();
    if let Some(dir) = args.next() {
        env::set_current_dir(&dir)?;
    }

    drop(args);

    let config: Config = serde_json::from_str(&fs::read_to_string(CONFIG_FILE)?)?;
    debug!("config: {config:#?}");

    let db_conn = Connection::open(DB_FILE)?;
    store::init_db(&db_conn)?;

    let mut app = match store::load_state(&db_conn) {
        Ok((state, api_state)) => App::load(state, api_state, db_conn)?,
        Err(e) => {
            warn!("load state from database: {e:#?}");
            let app = App::new(&refresh_token(&config)?, db_conn).await?;
            // Save initial state immediately so it exists in db
            store::save_state(&app.db, &app.state)?;
            store::save_token(&app.db, &app.api.state)?;
            app
        }
    };

    #[cfg(any(feature = "inotify", feature = "notify"))]
    drop(fs::File::create(NOTIFY_FILE)?);

    #[cfg(feature = "base-notify")]
    let (wakeup_tx, mut wakeup_rx) = tokio::sync::mpsc::channel::<()>(16);

    #[cfg(feature = "callback")]
    let mut callback = Callback::new();

    let http = reqwest::Client::new();

    let fetcher = config
        .pix_dir
        .as_ref()
        .map(|dir| (std::path::PathBuf::from(dir), DownloadClient::new()));

    let rx = Arc::new(Notify::new());
    let tx = rx.clone();
    ctrlc::set_handler(move || {
        warn!("shutting down");
        tx.notify_waiters();
    })?;

    #[cfg(feature = "inotify")]
    inotify::spawn(wakeup_tx.clone());

    #[cfg(feature = "notify")]
    notify::spawn(wakeup_tx.clone());

    #[cfg(feature = "tray")]
    tray::spawn(
        wakeup_tx.clone(),
        rx.clone(),
        config.python_site_packages.clone(),
    );
    let delay = Duration::from_secs(config.delay.into());
    let mut token = Default::default();

    loop {
        match app.refresh(&config).await {
            Err(e) => {
                error!("refresh failed: {e:#?}");
                token = Default::default();
            }
            Ok(mut illusts) => {
                debug!("refresh: {} illusts", illusts.len());
                // Before archiving (the URL diff needs the old metadata) and
                // before hooks (files must land before the announcement).
                if let Some((dir, dl)) = &fetcher {
                    fetch::process(dl, dir, &app.db, &mut illusts).await;
                }
                if config.archive {
                    store::archive_illusts(&app.db, &illusts)?;
                }
                let since = app.since();
                let ago = app.since_ago();
                if token != app.token() {
                    token = app.token();
                    info!(
                        "{}{}{} illusts since {} ({}, {})",
                        if app.remain { "> " } else { "" },
                        if app.skip { "~ " } else { "" },
                        app.dist,
                        since,
                        ago,
                        app.iid
                    );

                    #[cfg(feature = "tray")]
                    if let Err(e) = Python::attach(|py| {
                        PyModule::import(py, "tray")?
                            .getattr("update")?
                            .call1((app.dist,))?;
                        PyResult::Ok(())
                    }) {
                        error!("tray: {e:#?}");
                    }

                    #[cfg(feature = "request")]
                    if let Some(ref url) = config.notify_url {
                        let url = format!("{url}/{}", app.dist);
                        match http.get(&url).send().await {
                            Ok(resp) => finalize_http(resp).await,
                            Err(e) => error!("request: {e:#?}"),
                        }
                    }
                }

                // Outside the token check: metadata updates don't move (iid, dist).
                for url in &config.hooks {
                    if let Err(e) = hook::send_illusts(&http, url, &illusts, &app).await {
                        error!("hook {url}: {e:#?}");
                    }
                }

                #[cfg(feature = "callback")]
                if let Some(cb) = &mut callback {
                    let args = &[
                        cb.itoa.format(app.dist),
                        cb.itoa2.format(app.iid),
                        &app.since(),
                        &ago,
                        if app.remain { "1" } else { "0" },
                        if app.skip { "1" } else { "0" },
                    ];

                    if let Err(e) = do_callback(CALLBACK_FILE, args) {
                        error!("callback: {e:#?}");
                    }
                }
            }
        }

        #[cfg(feature = "base-notify")]
        tokio::select! {
            () = sleep(delay) => {},
            () = rx.notified() => {
                break;
            },
            Some(()) = wakeup_rx.recv() => {
                token = Default::default();
            }
        }

        #[cfg(not(feature = "base-notify"))]
        tokio::select! {
            () = sleep(delay) => {},
            () = rx.notified() => {
                break;
            }
        }
    }
    Ok(())
}
