use crate::NOTIFY_FILE;
use anyhow::Result;
use futures::StreamExt;
use inotify::{Inotify, WatchMask};
use log::{debug, error};

async fn run(tx: tokio::sync::mpsc::Sender<()>) -> Result<()> {
    let mut buf = [0; 128];
    let inotify_dep = Inotify::init()?;
    inotify_dep.watches().add(NOTIFY_FILE, WatchMask::OPEN)?;
    let mut stream = inotify_dep.into_event_stream(&mut buf)?;
    while let Some(event) = stream.next().await {
        match event {
            Ok(ev) => {
                debug!("inotify event: {ev:#?}");
                if tx.send(()).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                error!("inotify stream error: {e:#?}");
                break;
            }
        }
    }
    Ok(())
}

pub(crate) fn spawn(tx: tokio::sync::mpsc::Sender<()>) {
    tokio::spawn(async move {
        if let Err(e) = run(tx).await {
            error!("inotify watcher error: {e:#?}");
        }
    });
}
