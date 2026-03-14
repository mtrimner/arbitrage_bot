mod coinbase_ws;
mod config;
mod engine;
mod exec;
mod leadlag;
mod market_manager;
mod report;
mod state;
mod types;
mod ws;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use dotenv::dotenv;
use std::env;
use std::sync::Arc;

use config::Config;
use state::Shared;

use kalshi_rs::auth::Account;
use kalshi_rs::{KalshiClient, KalshiWebsocketClient};

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cfg = Config::from_env();
    info!(
        exec_mode = ?cfg.exec_mode,
        series = ?cfg.series_tickers,
        coinbase_ws = cfg.coinbase_ws_enabled,
        coinbase_product = %cfg.coinbase_product_id,
        "bot starting"
    );

    let api_key_id = env::var("API_KEY").expect("No API_KEY");
    let account = Account::from_file("./private_keys/kalshi_private.pem", api_key_id.as_str())?;

    let http = Arc::new(KalshiClient::new(account.clone()));
    let ws_client = KalshiWebsocketClient::new(account);

    let active = market_manager::bootstrap_active_markets(&http, &cfg.series_tickers).await?;

    let tickers: Vec<String> = active.iter().map(|m| m.market_ticker.clone()).collect();
    info!(tickers = ?tickers, "bootstrapped active markets");
    let shared = Shared::new(tickers.clone(), cfg.coinbase_product_id.clone());
    let leadlag = leadlag::new_shared_leadlag();
    market_manager::seed_shared_times(&shared, &active).await?;

    let (exec_tx, exec_rx) = mpsc::channel(256);
    let (ws_ctl_tx, ws_ctl_rx) = mpsc::channel(64);

    {
        let shared = shared.clone();
        let http = http.clone();
        let cfg = cfg.clone();
        let leadlag2 = leadlag.clone();
        tokio::spawn(async move {
            if let Err(e) =
                ws::task::run_ws(ws_client, http, cfg, shared, leadlag2, tickers, ws_ctl_rx).await
            {
                tracing::error!(err = %format!("{e:#}"), "ws task exited");
            }
        });
    }

    {
        let shared = shared.clone();
        let http = http.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = exec::task::run_exec(cfg, http, shared, exec_rx).await {
                tracing::error!(err = %format!("{e:#}"), "exec task exited");
            }
        });
    }

    {
        let shared = shared.clone();
        let http = http.clone();
        let cfg = cfg.clone();
        let ws_ctl_tx = ws_ctl_tx.clone();
        let exec_tx = exec_tx.clone();

        tokio::spawn(async move {
            if let Err(e) =
                market_manager::run_market_manager(cfg, http, shared, ws_ctl_tx, exec_tx, active)
                    .await
            {
                tracing::error!(err = %format!("{e:#}"), "market manager exited");
            }
        });
    }

    if !cfg.coinbase_ws_enabled {
        tracing::warn!("COINBASE_WS disabled; signal-driven strategy will stay idle");
    }

    let _coinbase_rx = if cfg.coinbase_ws_enabled {
        let rx = coinbase_ws::spawn_coinbase_ticker(
            cfg.coinbase_product_id.clone(),
            cfg.clone(),
            shared.clone(),
        );
        let rx_clone = rx.clone();
        coinbase_ws::spawn_coinbase_logger(rx.clone(), cfg.coinbase_log_delta_usd);

        if cfg.coinbase_leadlag_enabled {
            let shared = shared.clone();
            let leadlag = leadlag.clone();
            let cfg2 = cfg.clone();

            tokio::spawn(async move {
                coinbase_ws::spawn_coinbase_move_detector(rx_clone, cfg2, shared, leadlag).await;
            });
        }

        Some(rx)
    } else {
        None
    };

    engine::task::run_engine(cfg, shared, exec_tx).await?;

    Ok(())
}
