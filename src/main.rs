// mod types;
// mod state;
// mod ws;
// mod engine;
// mod config;
// mod exec;


// use anyhow::{Result, Context};
// use tokio::sync::mpsc;
// use tracing_subscriber::EnvFilter;

// use state::Shared;
// use config::Config;
// use engine::task;

// use kalshi_rs::{KalshiClient, KalshiWebsocketClient};
// use kalshi_rs::auth::Account;
// use kalshi_rs::markets::models::MarketsQuery;


// #[tokio::main]
// async fn main() -> Result<()> {
//     tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    
//     let cfg = Config::default();

//     let prod_api_key_id = "089ba1e1-1eec-4086-8753-3caf64d2cc1c";
//     let account = Account::from_file("prod_kalshi_private.pem", prod_api_key_id)?;
//     let client = KalshiClient::new(account.clone());
//     let ws_client = KalshiWebsocketClient::new(account);

//     let params = MarketsQuery {
//     series_ticker: Some("KXBTC15M".into()),
//     ..Default::default()
//     };

//     let markets = client.get_all_markets(&params).await?;

//     let active_event = markets.markets
//         .into_iter()
//         .find(|m| m.status == "active")
//         .context("No active event found")?;

//     let tickers = vec![active_event.ticker.to_string()];
//     let shared = Shared::new(tickers.clone());
    
//     let shared = shared.clone();
//     let tickers = tickers.clone();
//     let (tx, rx) = mpsc::channel(64);
    
//     {
//         let shared = shared.clone();
//         let tickers = tickers.clone();
//         let client = client.clone();
//         tokio::spawn(async move {
//             let _ = ws::task::run_ws(ws_client, client, shared, tickers).await;
//         });
//     }

//     {
//         let shared = shared.clone();
//         tokio::spawn(async move {
//             let _ = exec::task::run_exec(client, shared, rx).await;
//         });
//     }

//     engine::task::run_engine(cfg, shared, tx).await;
//     Ok(())
// }

mod types;
mod state;
mod ws;
mod engine;
mod config;
mod exec;
mod market_manager;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use std::sync::Arc;
use dotenv::dotenv;
use std::env;

use state::Shared;
use config::Config;

use kalshi_rs::{KalshiClient, KalshiWebsocketClient};
use kalshi_rs::auth::Account;
use kalshi_rs::markets::models::MarketsQuery;

#[tokio::main]
async fn main() -> Result<()> {
    // Basic logging: set RUST_LOG=info (or debug) to see output.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    dotenv().ok();

    let cfg = Config::default();

   
    let api_key_id = env::var("API_KEY").expect("No API_KEY");
    let account = Account::from_file("./private_keys/kalshi_private.pem", api_key_id.as_str())?;

    // KalshiClient is NOT Clone in your build, so we wrap it in Arc.
    let http = Arc::new(KalshiClient::new(account.clone()));
    let ws_client = KalshiWebsocketClient::new(account);

    // Bootstrap: one active market per series
    let active = market_manager::bootstrap_active_markets(&http, &cfg.series_tickers).await?;
    // println!("Active Tickers: {:#?}", active);
    // Create Shared with all current active tickers (so engine/ws start correct)
    let tickers: Vec<String> = active.iter().map(|m| m.market_ticker.clone()).collect();
    let shared = Shared::new(tickers.clone());
    // println!("Tickers: {:#?}", tickers);
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
    // {
    //     let shared = shared.clone();
    //     let http = http.clone();
    //     tokio::spawn(async move {
    //         let _ = exec::task::run_exec(http, shared, exec_rx).await;
    //     });
    // }

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
    // ws::task::run_ws(ws_client, http, cfg, shared, tickers, ws_ctl_rx).await?;

    Ok(())
}
