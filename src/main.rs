#[macro_use]
extern crate log;

use std::collections::BTreeSet;
use std::io::{Seek, Write};
use std::ops::{Deref, DerefMut};
use std::process::Command;
use std::sync::Arc;
use std::{env, fs};

use anyhow::{Result, bail};
use pixiv::aapi::Restrict;
use pixiv::client::{AuthedClient, AuthedState};
use pixiv::download::DownloadClient;
use pixiv::model::IllustId;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, format_description, macros::format_description};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};

#[cfg(feature = "inotify")]
use futures::{FutureExt, StreamExt};

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
const CALLBACK_FILE: &str = "./callback";
const IMG_FILE: &str = "img.jpg";
const STATE_FILE: &str = "state.json";

#[cfg(feature = "inotify")]
const NOTIFY_FILE: &str = "notify";

#[derive(Deserialize, Debug, Clone)]
struct Config {
    refresh_token: String,
    #[serde(default = "default_delay")]
    delay: u32,
    #[serde(default = "default_max_pages")]
    max_pages: u32,
    #[serde(default = "default_min_skip_pages")]
    min_skip_pages: u32,
    notify_url: Option<String>,
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
    illusts: Vec<Illust>,
    next_url: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct AppState {
    iid: IllustId,
    since: OffsetDateTime,
    remain: bool,
    skip: bool,
    vis: BTreeSet<IllustId>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            iid: 0,
            since: OffsetDateTime::UNIX_EPOCH,
            remain: false,
            skip: false,
            vis: BTreeSet::new(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct AppDump {
    api: AuthedState,
    #[serde(flatten)]
    state: AppState,
}

struct App {
    api: AuthedClient,
    state: AppState,
    downloader: DownloadClient,
    tz: UtcOffset,
    ago: timeago::Formatter,
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
    async fn new(refresh_token: &str) -> Result<Self> {
        Ok(Self {
            api: AuthedClient::new(refresh_token).await?,
            state: AppState::default(),
            downloader: DownloadClient::new(),
            tz: UtcOffset::current_local_offset()?,
            ago: timeago::Formatter::new(),
        })
    }

    fn load(dump: AppDump) -> Result<Self> {
        Ok(Self {
            api: AuthedClient::load(dump.api),
            state: dump.state,
            downloader: DownloadClient::new(),
            tz: UtcOffset::current_local_offset()?,
            ago: timeago::Formatter::new(),
        })
    }

    fn dump(self) -> AppDump {
        AppDump {
            api: self.api.state,
            state: self.state,
        }
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

    async fn refresh(&mut self, config: &Config) -> Result<()> {
        self.api.ensure_authed().await?;
        let mut r: Page = self.api.illust_follow(Restrict::Public).await?;

        let mut pn = 1;
        let mut ids = BTreeSet::new();
        loop {
            debug!("page {} has {} illusts", pn, r.illusts.len());
            let mut may_skip = pn >= config.min_skip_pages;
            for illust in r.illusts {
                if illust.is_bookmarked {
                    debug!("bookmarked: {illust:#?}");
                    if self.iid != illust.id {
                        debug!("new id: {} time: {}", illust.id, illust.create_date);
                        self.since = self.convert_date(&illust.create_date)?;

                        let mut image = self
                            .downloader
                            .download(&illust.image_urls.square_medium)
                            .await?;
                        let mut file = fs::File::create(IMG_FILE)?;

                        while let Some(chunk) = image.chunk().await? {
                            file.write_all(&chunk)?;
                        }
                        debug!("downloaded {} bytes", file.stream_position()?);

                        self.iid = illust.id;
                    }
                    self.remain = false;
                    self.skip = false;
                    self.vis = ids;
                    return Ok(());
                }
                ids.insert(illust.id);
                if may_skip && !self.vis.contains(&illust.id) {
                    may_skip = false;
                }
            }
            if may_skip {
                if !self.skip {
                    warn!("skipping from page {pn}");
                    self.skip = true;
                }
                self.vis.extend(ids.into_iter());
                return Ok(());
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
            self.vis.extend(ids.into_iter());
            return Ok(());
        }
    }

    fn dist(&self) -> usize {
        self.vis.len()
    }

    fn token(&self) -> (IllustId, usize) {
        (self.iid, self.dist())
    }
}

fn notify(bin: &str, args: &[&str]) -> Result<()> {
    debug!("notify: {bin} {args:?}");
    let r = Command::new(bin).args(args).spawn()?.wait()?;
    if r.success() {
        debug!("notify: returned {:?}", r.code());
    } else {
        bail!("returned {:?}", r.code());
    }
    Ok(())
}

fn load_state(path: &str) -> Result<App> {
    App::load(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn bye(app: App) -> Result<()> {
    info!("dumping state");
    fs::write(STATE_FILE, serde_json::to_string_pretty(&app.dump())?)?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var("RUST_LOG", "info");
        }
    }
    pretty_env_logger::init_timed();

    if let Some(dir) = env::args().nth(1) {
        env::set_current_dir(&dir)?;
    }

    let config: Config = serde_json::from_str(&fs::read_to_string(CONFIG_FILE)?)?;
    debug!("config: {config:#?}");

    let mut app = match load_state(STATE_FILE) {
        Ok(app) => app,
        Err(e) => {
            warn!("load state: {e:#?}");
            App::new(&config.refresh_token).await?
        }
    };

    #[cfg(feature = "inotify")]
    let mut buf = [0; 128];

    #[cfg(feature = "inotify")]
    let mut inotify = {
        drop(fs::File::create(NOTIFY_FILE)?);
        use inotify::{Inotify, WatchMask};
        let inotify = Inotify::init()?;
        inotify.watches().add(NOTIFY_FILE, WatchMask::OPEN)?;
        inotify.into_event_stream(&mut buf)?
    };

    let mut exec = false;
    if let Ok(metadata) = fs::metadata(CALLBACK_FILE) {
        if metadata.is_file() && {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            true
        } {
            exec = true;
        }
    }

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

    let delay = Duration::from_secs(config.delay.into());
    let mut token = Default::default();
    let mut itoa = itoa::Buffer::new();
    let mut itoa2 = itoa::Buffer::new();
    loop {
        if let Err(e) = app.refresh(&config).await {
            error!("refresh failed: {e:#?}");
            token = Default::default();
        } else {
            let since = app.since();
            let ago = app.since_ago();
            if token != app.token() {
                token = app.token();
                info!(
                    "{}{}{} illusts since {} ({}, {})",
                    if app.remain { "> " } else { "" },
                    if app.skip { "~ " } else { "" },
                    app.dist(),
                    since,
                    ago,
                    app.iid
                );
            }

            if exec {
                let args = &[
                    itoa.format(app.dist()),
                    itoa2.format(app.iid),
                    &app.since(),
                    &ago,
                    if app.remain { "1" } else { "0" },
                    if app.skip { "1" } else { "0" },
                ];

                if let Err(e) = notify(CALLBACK_FILE, args) {
                    error!("callback: {e:#?}");
                }
            }

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
        }

        #[cfg(feature = "inotify")]
        while let Some(e) = inotify.next().now_or_never() {
            info!("inotify: {:#?}", e);
        }

        #[cfg(feature = "inotify")]
        tokio::select! {
            () = sleep(delay) => {},
            () = rx.notified() => {
                return bye(app);
            },
            r = inotify.next() => {
                match r {
                    Some(Ok(_)) => {
                        info!("refreshing");
                    }
                    r => {
                        bail!("inotify: {:?}", r);
                    }
                }
                token = Default::default();
            }
        }

        #[cfg(not(feature = "inotify"))]
        tokio::select! {
            () = sleep(delay) => {},
            () = rx.notified() => {
                return bye(app);
            },
        }
    }
}
