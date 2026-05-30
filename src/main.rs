#[macro_use]
extern crate log;

mod store;

use std::collections::BTreeSet;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{env, fs};

use anyhow::Result;
use pixiv::aapi::Restrict;
use pixiv::client::{AuthedClient, AuthedState};
use pixiv::model::IllustId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, format_description, macros::format_description};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};

#[cfg(feature = "download")]
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

#[cfg(feature = "hook")]
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
    refresh_token: String,
    #[serde(default = "default_delay")]
    delay: u32,
    #[serde(default = "default_max_pages")]
    max_pages: u32,
    #[serde(default = "default_min_skip_pages")]
    min_skip_pages: u32,
    #[serde(default)]
    archive: bool,
    #[cfg(feature = "request")]
    notify_url: Option<String>,
    #[cfg(feature = "tray")]
    python_site_packages: Option<String>,
    #[cfg(feature = "hook")]
    hook: Option<String>,
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
pub(crate) struct AppState {
    pub(crate) iid: IllustId,
    pub(crate) since: OffsetDateTime,
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

impl App {
    async fn new(refresh_token: &str, db: Connection) -> Result<Self> {
        Ok(Self {
            api: AuthedClient::new(refresh_token).await?,
            state: AppState::default(),
            #[cfg(feature = "download")]
            downloader: DownloadClient::new(),
            tz: UtcOffset::current_local_offset()?,
            ago: timeago::Formatter::new(),
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

    async fn refresh(
        &mut self,
        config: &Config,
    ) -> Result<Vec<(IllustId, Box<serde_json::value::RawValue>)>> {
        if self.api.ensure_authed().await? {
            store::save_token(&self.db, &self.api.state)?;
        }
        let mut r: Page = self.api.illust_follow(Restrict::Public).await?;

        let mut pn = 1;
        let mut ids = BTreeSet::new();
        let mut new_illusts = Vec::new();
        loop {
            debug!("page {} has {} illusts", pn, r.illusts.len());
            let mut may_skip = pn >= config.min_skip_pages;
            for illust_val in r.illusts {
                let illust: Illust = serde_json::from_str(illust_val.get())?;

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
                    self.remain = false;
                    self.skip = false;
                    store::reset_seen(&self.db, &ids)?;
                    return Ok(new_illusts);
                }

                if !store::is_seen(&self.db, illust.id).unwrap_or(false) {
                    new_illusts.push((illust.id, illust_val));
                }

                ids.insert(illust.id);
                if may_skip && !store::is_seen(&self.db, illust.id).unwrap_or(false) {
                    may_skip = false;
                }
            }
            if may_skip {
                if !self.skip {
                    warn!("skipping from page {pn}");
                    self.skip = true;
                }
                store::extend_seen(&self.db, &ids)?;
                return Ok(new_illusts);
            }
            if let Some(url) = r.next_url {
                if pn >= config.max_pages {
                    if !self.remain {
                        warn!("reached max pages {pn}");
                        self.remain = true;
                    }
                } else {
                    r = self.api.call_url(&url).await?;
                    pn += 1;
                    continue;
                }
            } else {
                warn!("no more pages");
                self.remain = false;
                self.skip = false;
            }
            store::extend_seen(&self.db, &ids)?;
            return Ok(new_illusts);
        }
    }

    fn dist(&self) -> usize {
        store::get_seen_count(&self.db).unwrap_or(0)
    }

    fn token(&self) -> (IllustId, usize) {
        (self.iid, self.dist())
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
            let app = App::new(&config.refresh_token, db_conn).await?;
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

    #[cfg(feature = "request")]
    let request = config
        .notify_url
        .as_ref()
        .map(|url| (reqwest::Client::new(), url));

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
            Ok(new_illusts) => {
                debug!("refresh: illusts: {new_illusts:#?}");
                if config.archive {
                    store::archive_illusts(&app.db, &new_illusts)?;
                }
                let since = app.since();
                let ago = app.since_ago();
                if token != app.token() {
                    token = app.token();
                    let status = format!(
                        "{}{}{} illusts since {} ({}, {})",
                        if app.remain { "> " } else { "" },
                        if app.skip { "~ " } else { "" },
                        app.dist(),
                        since,
                        ago,
                        app.iid
                    );
                    info!("{status}");

                    #[cfg(feature = "tray")]
                    if let Err(e) = Python::attach(|py| {
                        PyModule::import(py, "tray")?
                            .getattr("update")?
                            .call1((app.dist(),))?;
                        PyResult::Ok(())
                    }) {
                        error!("tray: {e:#?}");
                    }

                    #[cfg(feature = "request")]
                    if let Some((client, url)) = &request {
                        let url = format!("{url}/{}", app.dist());
                        match client.get(&url).send().await {
                            Ok(resp) => {
                                let status = resp.status();
                                if !status.is_success() {
                                    error!("request: {status}: {}", resp.text().await?);
                                }
                            }
                            Err(e) => error!("send request: {e:#?}"),
                        }
                    }

                    #[cfg(feature = "hook")]
                    if let Some(ref url) = config.hook
                        && !new_illusts.is_empty()
                        && let Err(e) = hook::send_illusts(url, &new_illusts, &status).await
                    {
                        error!("hook: {e:#?}");
                    }
                }

                #[cfg(feature = "callback")]
                if let Some(cb) = &mut callback {
                    let args = &[
                        cb.itoa.format(app.dist()),
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
