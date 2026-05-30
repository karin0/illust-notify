use anyhow::Result;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::Notify;

#[cfg(windows)]
pub(crate) fn show_window(hide: bool) {
    // Only works for conhost
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW, ShowWindow};
    unsafe {
        let handle = GetConsoleWindow();
        if handle.is_null() {
            log::warn!("no console window");
        } else {
            let cmd = if hide { SW_HIDE } else { SW_SHOW };
            let r = ShowWindow(handle, cmd);
            log::info!("ShowWindow: {handle:?} {cmd:?} {r:?}");
        }
    }
}

async fn run(
    tx: tokio::sync::mpsc::Sender<()>,
    shutdown: Arc<Notify>,
    python_site_packages: Option<String>,
) -> Result<()> {
    use pyo3::prelude::*;
    use pyo3::types::{PyCFunction, PyDict, PyTuple};

    Python::initialize();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<i32>(16);

    let callback = move |args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>| {
        let arg = args
            .get_item(0)
            .ok()
            .and_then(|v| v.extract::<i32>().ok())
            .unwrap_or(0);
        debug!("tray callback: {arg:?}");
        let _ = event_tx.blocking_send(arg);
    };

    Python::attach(|py| {
        let sys = py.import("sys")?;
        let version = sys.getattr("version")?;
        info!("Python: {version}");

        let path = sys.getattr("path")?;

        if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
            let site_packages = format!("{venv}/Lib/site-packages");
            path.call_method1("insert", (0, site_packages))?;
        } else if let Some(site_packages) = &python_site_packages {
            path.call_method1("insert", (0, site_packages))?;
        }

        path.call_method1("insert", (0, "."))?;

        let callback = PyCFunction::new_closure(py, None, None, callback)?;
        PyModule::import(py, "tray")?.call_method1("start", (callback,))?;
        PyResult::Ok(())
    })?;

    while let Some(arg) = event_rx.recv().await {
        match arg {
            1 => {
                let _ = tx.send(()).await;
            }
            #[cfg(windows)]
            2 => show_window(true),
            #[cfg(windows)]
            3 => show_window(false),
            _ => {
                shutdown.notify_waiters();
                break;
            }
        }
    }
    Ok(())
}

pub(crate) fn spawn(
    tx: tokio::sync::mpsc::Sender<()>,
    shutdown: Arc<Notify>,
    python_site_packages: Option<String>,
) {
    #[cfg(windows)]
    {
        if std::env::args().any(|s| s == "-d") {
            show_window(true);
        }
    }

    tokio::spawn(async move {
        if let Err(e) = run(tx, shutdown, python_site_packages).await {
            error!("tray watcher error: {e:#?}");
        }
    });
}
