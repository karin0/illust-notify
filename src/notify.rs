use crate::NOTIFY_FILE;
use anyhow::Result;
use log::{debug, error};
use notify::{Event, RecursiveMode, Result as NotifyResult, Watcher as _, recommended_watcher};

async fn run(tx: tokio::sync::mpsc::Sender<()>) -> Result<()> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
    let mut watcher = recommended_watcher(move |res: NotifyResult<Event>| match res {
        Ok(event) => {
            debug!("notify event: {event:#?}");
            if let Err(e) = event_tx.blocking_send(event) {
                error!("send notify event: {e:#?}");
            }
        }
        Err(e) => error!("notify watcher error: {e:#?}"),
    })?;

    watcher.watch(NOTIFY_FILE.as_ref(), RecursiveMode::NonRecursive)?;

    while event_rx.recv().await.is_some() {
        if tx.send(()).await.is_err() {
            break;
        }
    }
    Ok(())
}

pub(crate) fn spawn(tx: tokio::sync::mpsc::Sender<()>) {
    tokio::spawn(async move {
        if let Err(e) = run(tx).await {
            error!("notify watcher error: {e:#?}");
        }
    });
}
