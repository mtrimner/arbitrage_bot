use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::config::Config;
use crate::state::Shared;
use crate::types::ExecCommand;

pub async fn run_engine(cfg: Config, shared: Shared, tx: mpsc::Sender<ExecCommand>) -> Result<()> {
    let mut interval = time::interval(Duration::from_millis(cfg.tick_ms));

    loop {
        let interval_fired = tokio::select! {
            _ = interval.tick() => true,
            _ = shared.notify.notified() => false,
        };
        for item in shared.tickers.iter() {
            let ticker = item.key().clone();
            let ts = item.value().clone();

            // If interval ticked, run “housekeeping” even if no new data.
            // If notified, we can skip when not dirty.
            if !interval_fired && !ts.take_dirty() {
                continue;
            } else {
                // if interval fired, clear dirty anyway so we don’t keep re-running
                ts.take_dirty();
            }

            let cmd = {
                let mut g = ts.mkt.write().await;
                crate::engine::decision::decide(&cfg, &ticker, &mut g)
            };

            if let Some(cmd) = cmd {
                let _ = tx.try_send(cmd);
            }
        }
    }
}
