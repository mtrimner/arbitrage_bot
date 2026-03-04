use anyhow::{Result, anyhow};
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
            let dirty = ts.take_dirty();
            // If interval ticked, run “housekeeping” even if no new data.
            // If notified, we can skip when not dirty.
            if !interval_fired && !dirty {
                continue;
            }

            let cmd = {
                let mut g = ts.mkt.write().await;
                crate::engine::decision::decide(&cfg, &ticker, &mut g)
            };

            if let Some(cmd) = cmd {
                if let Err(e) = tx.send(cmd).await {
                    return Err(anyhow!("exec channel closed: {e}"));
                }
                
            }
        }
    }
}
