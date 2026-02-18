mod types;
mod state;
mod ws;
mod engine;
mod config;
mod exec;
mod market_manager;
mod report;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use std::sync::Arc;
use dotenv::dotenv;
use std::env;

use state::Shared;
use config::{Config, ExecMode};

use kalshi_rs::{KalshiClient, KalshiWebsocketClient};
use kalshi_rs::auth::Account;


#[tokio::main]
async fn main() -> Result<()> {
    // Basic logging: set RUST_LOG=info (or debug) to see output.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    dotenv().ok();

    let mut cfg = Config::default();
    let mode = std::env::var("EXEC_MODE").unwrap_or_else(|_| "paper".to_string());
    cfg.exec_mode = match mode.to_lowercase().as_str() {
        "paper" | "dry" | "sim" => ExecMode::Paper,
        _ => ExecMode::Live,
    };

    let api_key_id = env::var("API_KEY").expect("No API_KEY");
    let account = Account::from_file("./private_keys/kalshi_private.pem", api_key_id.as_str())?;

    // KalshiClient is NOT Clone in your build, so we wrap it in Arc.
    let http = Arc::new(KalshiClient::new(account.clone()));
    let ws_client = KalshiWebsocketClient::new(account);

    // Bootstrap: one active market per series
    let active = market_manager::bootstrap_active_markets(&http, &cfg.series_tickers).await?;

    // Create Shared with all current active tickers (so engine/ws start correct)
    let tickers: Vec<String> = active.iter().map(|m| m.market_ticker.clone()).collect();
    let shared = Shared::new(tickers.clone());

    // Seed close_ts/open_ts into Market state for each ticker
    market_manager::seed_shared_times(&shared, &active).await?;

    // Exec channel (engine + market_manager can both send ExecCommand)
    let (exec_tx, exec_rx) = mpsc::channel(256);

    // WS control channel (market_manager -> ws task)
    let (ws_ctl_tx, ws_ctl_rx) = mpsc::channel(64);


    // WS task
    {
        let shared = shared.clone();
        let http = http.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let _ = ws::task::run_ws(ws_client, http, cfg, shared, tickers, ws_ctl_rx).await;
        });
    }

    // Exec task
    {
        let shared = shared.clone();
        let http = http.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let _ = exec::task::run_exec(cfg, http, shared, exec_rx).await;
        });
    }

    // Market manager task (rotates tickers based on close_time)
    {
        let shared = shared.clone();
        let http = http.clone();
        let cfg = cfg.clone();
        let ws_ctl_tx = ws_ctl_tx.clone();
        let exec_tx = exec_tx.clone();

        tokio::spawn(async move {
            let _ = market_manager::run_market_manager(
                cfg,
                http,
                shared,
                ws_ctl_tx,
                exec_tx,
                active,
            ).await;
        });
    }

    // Engine runs on the main task
    engine::task::run_engine(cfg, shared, exec_tx).await?;

    Ok(())
}
