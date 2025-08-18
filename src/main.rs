#[macro_use]
extern crate log;

use std::collections::BTreeSet;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{env, fs};

use anyhow::Result;
use pixiv::aapi::Restrict;
use pixiv::client::{AuthedClient, AuthedState};
use pixiv::model::IllustId;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, format_description, macros::format_description};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};

#[cfg(feature = "download")]
use pixiv::download::DownloadClient;

#[cfg(feature = "inotify")]
use futures::{FutureExt, StreamExt};

#[cfg(feature = "tray")]
use pyo3::prelude::*;

#[cfg(any(
    all(feature = "inotify", feature = "notify"),
    all(feature = "inotify", feature = "tray"),
    all(feature = "notify", feature = "tray")
))]
compile_error!("features conflict");

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
const STATE_FILE: &str = "state.json";

#[cfg(feature = "callback")]
const CALLBACK_FILE: &str = "./callback";

#[cfg(feature = "download")]
const IMG_FILE: &str = "img.jpg";

#[cfg(any(feature = "inotify", feature = "notify"))]
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
    #[cfg(feature = "download")]
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
            #[cfg(feature = "download")]
            downloader: DownloadClient::new(),
            tz: UtcOffset::current_local_offset()?,
            ago: timeago::Formatter::new(),
        })
    }

    fn load(dump: AppDump) -> Result<Self> {
        Ok(Self {
            api: AuthedClient::load(dump.api),
            state: dump.state,
            #[cfg(feature = "download")]
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

fn load_state(path: &str) -> Result<App> {
    App::load(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn bye(app: App) -> Result<()> {
    info!("dumping state");
    fs::write(STATE_FILE, serde_json::to_string_pretty(&app.dump())?)?;
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();

    let mut args = env::args();
    args.next();
    if let Some(dir) = args.next() {
        env::set_current_dir(&dir)?;
    }

    #[cfg(windows)]
    let daemon = args.any(|s| s == "-d");
    drop(args);

    #[cfg(feature = "tray")]
    let mut config: Config = serde_json::from_str(&fs::read_to_string(CONFIG_FILE)?)?;

    #[cfg(not(feature = "tray"))]
    let config: Config = serde_json::from_str(&fs::read_to_string(CONFIG_FILE)?)?;
    debug!("config: {config:#?}");

    let mut app = match load_state(STATE_FILE) {
        Ok(app) => app,
        Err(e) => {
            warn!("load state: {e:#?}");
            App::new(&config.refresh_token).await?
        }
    };

    #[cfg(any(feature = "inotify", feature = "notify"))]
    drop(fs::File::create(NOTIFY_FILE)?);

    #[cfg(feature = "inotify")]
    let mut buf = [0; 128];

    #[cfg(feature = "inotify")]
    let mut inotify = {
        use inotify::{Inotify, WatchMask};
        let inotify = Inotify::init()?;
        inotify.watches().add(NOTIFY_FILE, WatchMask::OPEN)?;
        inotify.into_event_stream(&mut buf)?
    };

    #[cfg(feature = "notify")]
    let (_notify, mut notify) = {
        use notify::{Event, RecursiveMode, Result, Watcher, recommended_watcher};

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut watcher = recommended_watcher(move |res: Result<Event>| match res {
            Ok(event) => {
                debug!("notify: {event:#?}");
                futures::executor::block_on(async {
                    if let Err(e) = tx.send(event).await {
                        error!("send notify: {e:#?}");
                    }
                });
            }
            Err(e) => error!("notify: {e:#?}"),
        })?;

        watcher.watch(NOTIFY_FILE.as_ref(), RecursiveMode::NonRecursive)?;
        (watcher, rx)
    };

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

    #[cfg(feature = "tray")]
    let tray_rx = {
        use pyo3::types::{PyCFunction, PyDict, PyTuple};

        pyo3::prepare_freethreaded_python();

        let tx = rx.clone();
        let exit = move |_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>| {
            warn!("exiting from tray");
            tx.notify_waiters();
        };

        let rx = Arc::new(Notify::new());
        let tx = rx.clone();

        let notify = move |_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>| {
            tx.notify_waiters();
        };

        Python::with_gil(|py| {
            let sys = py.import("sys")?;
            let version = sys.getattr("version")?;
            info!("Python: {version}");

            let path = sys.getattr("path")?;

            if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
                let site_packages = format!("{venv}/Lib/site-packages");
                path.call_method1("insert", (0, site_packages))?;
            } else if let Some(python_site_packages) = config.python_site_packages.take() {
                path.call_method1("insert", (0, python_site_packages))?;
            }

            path.call_method1("insert", (0, "."))?;

            let exit = PyCFunction::new_closure(py, None, None, exit)?;
            let notify = PyCFunction::new_closure(py, None, None, notify)?;

            PyModule::import(py, "tray")?.call_method1("start", (exit, notify))?;
            PyResult::Ok(())
        })?;
        rx
    };

    let delay = Duration::from_secs(config.delay.into());
    let mut token = Default::default();

    #[cfg(windows)]
    if daemon {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, SW_HIDE, ShowWindow,
        };
        unsafe {
            let handle = GetForegroundWindow();
            if handle.is_null() {
                warn!("no foreground window found");
            } else {
                let r = ShowWindow(handle, SW_HIDE);
                info!("hiding console window: {handle:?} {r:?}");
            }
        }
    }

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

                #[cfg(feature = "tray")]
                if let Err(e) = Python::with_gil(|py| {
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

        #[cfg(feature = "inotify")]
        while let Some(e) = inotify.next().now_or_never() {
            debug!("inotify omiting: {e:#?}");
        }

        #[cfg(feature = "notify")]
        while let Ok(event) = notify.try_recv() {
            debug!("notify omitting: {event:#?}");
        }

        macro_rules! do_select {
            (
                $token:ident,
                $res0:tt = $src0:expr => $body0:block,
                $res1:tt = $src1:expr => $body1:block
                $(, $feature:literal: $result:tt = $source:expr => $body:block)+
            ) => {
                $(
                    #[cfg(feature = $feature)]
                    tokio::select! {
                        $res0 = $src0 => $body0,
                        $res1 = $src1 => $body1,
                        $result = $source => {
                            $body
                            $token = Default::default();
                        }
                    }
                )+
            };
        }

        do_select!(
            token,
            () = sleep(delay) => {},
            () = rx.notified() => {
                return bye(app);
            },
            "inotify": r = inotify.next() => {
                if let Some(Ok(event)) = r {
                    debug!("inotify: {event:#?}");
                } else {
                    error!("inotify: {r:#?}");
                    return bye(app);
                }
            },
            "notify": r = notify.recv() => {
                if let Some(event) = r {
                    debug!("notify got: {event:#?}");
                } else {
                    error!("notify closed");
                    return bye(app);
                }
            },
            "tray": () = tray_rx.notified() => {}
        );
    }
}
