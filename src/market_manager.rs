//! market_manager.rs
//!
//! Responsible for “control plane” market selection & rotation.
//!
//! - Uses REST (get_all_markets for each series_ticker) to find the active market.
//! - Parses open_time/close_time and stores them (as epoch seconds) in Market state.
//! - When a market closes, it fetches the new active market and tells the WS task to
//!   update subscriptions (add new ticker, delete old ticker).
//!
//! We do ONE window at a time per series (no overlap).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::mpsc, time::{self, Duration}};
use tracing::info;

use kalshi_rs::KalshiClient;
use kalshi_rs::markets::models::MarketsQuery;

use crate::config::Config;
use crate::state::Shared;
use crate::types::{ExecCommand, Side, WsMarketCommand};


#[derive(Debug, Clone)]
pub struct ActiveMarketMeta {
    pub series_ticker: String,
    pub market_ticker: String,
    pub open_ts: i64,
    pub close_ts: i64,
}

/// Parse RFC3339 timestamps like "2026-01-27T23:15:00Z" into epoch seconds (UTC).
fn parse_rfc3339_utc(ts: &str) -> Result<i64> {
    let dt = DateTime::parse_from_rfc3339(ts)?;
    Ok(dt.with_timezone(&Utc).timestamp())
}

/// Fetch the currently active market for a series (e.g. KXBTC15M).
/// If none are active (rare), picks the soonest future market by open_time.
pub async fn fetch_current_market(http: &KalshiClient, series_ticker: &str) -> Result<ActiveMarketMeta> {
    let params = MarketsQuery {
        series_ticker: Some(series_ticker.to_string()),
        ..Default::default()
    };

    let resp = http.get_all_markets(&params).await?;
    let markets = resp.markets;

    let now = Utc::now().timestamp();

    // 1) Prefer status == "active"
    if let Some(m) = markets.iter().find(|m| m.status == "active") {
        let open_ts = parse_rfc3339_utc(&m.open_time)?;
        let close_ts = parse_rfc3339_utc(&m.close_time)?;
        return Ok(ActiveMarketMeta {
            series_ticker: series_ticker.to_string(),
            market_ticker: m.ticker.to_string(),
            open_ts,
            close_ts,
        });
    }

    // 2) Fallback: pick soonest future market by open_time
    let mut best: Option<(i64, i64, String)> = None; // (open_ts, close_ts, ticker)
    for m in markets.iter() {
        let open_ts = match parse_rfc3339_utc(&m.open_time) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if open_ts < now { continue; }

        let close_ts = match parse_rfc3339_utc(&m.close_time) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match &best {
            None => best = Some((open_ts, close_ts, m.ticker.to_string())),
            Some((best_open, _, _)) => {
                if open_ts < *best_open {
                    best = Some((open_ts, close_ts, m.ticker.to_string()));
                }
            }
        }
    }

    let (open_ts, close_ts, ticker) = best.context("No active market and no upcoming market found")?;
    Ok(ActiveMarketMeta {
        series_ticker: series_ticker.to_string(),
        market_ticker: ticker,
        open_ts,
        close_ts,
    })
}

pub async fn bootstrap_active_markets(
    http: &KalshiClient,
    series_tickers: &[String],
) -> Result<Vec<ActiveMarketMeta>> {
    let mut out = Vec::with_capacity(series_tickers.len());
    for s in series_tickers {
        let cur = fetch_current_market(http, s).await?;
        out.push(cur);
    }
    Ok(out)
}

/// Write open_ts / close_ts into the live per-ticker Market state.
/// This is what the engine uses for time-remaining and mode selection.
pub async fn seed_shared_times(shared: &Shared, markets: &[ActiveMarketMeta]) -> Result<()> {
    info!("Seeding shared times: {:#?} - {:#?}", shared, markets);
    for m in markets {
        let ts = shared.ensure_ticker(&m.market_ticker);

        // Store timing info into live Market state
        {
            let mut g = ts.mkt.write().await;
            g.open_ts = Some(m.open_ts);
            g.close_ts = Some(m.close_ts);
        }

        ts.mark_dirty();
        shared.notify.notify_one();
    }
    Ok(())
}

/// Optional helper: cancel any known resting orders on a ticker before we drop it.
/// This is “nice to have”. If you don’t want cancels, you can remove this.
async fn cancel_known_resting(exec_tx: &mpsc::Sender<ExecCommand>, shared: &Shared, ticker: &str) {
    let Some(ts) = shared.tickers.get(ticker) else { return; };
    let mut g = ts.mkt.write().await;

    // If your RestingHint stores order_id, cancel them.
    // (If order_id is None because we never got ack, we can’t cancel by order_id.)
    let mut cancels = Vec::new();

    if let Some(h) = g.resting_yes.as_ref() {
        if let Some(oid) = h.order_id.as_ref() {
            cancels.push(oid.clone());
        }
    }
    if let Some(h) = g.resting_no.as_ref() {
        if let Some(oid) = h.order_id.as_ref() {
            cancels.push(oid.clone());
        }
    }

    // Clear hints so engine won’t keep acting on them.
    g.resting_yes = None;
    g.resting_no = None;

    drop(g);

    for oid in cancels {
        let _ = exec_tx.send(ExecCommand::CancelOrder {
            ticker: ticker.to_string(),
            order_id: oid,
        }).await;
    }
}

/// Main loop: watch current close_ts per series and rotate when closed.
pub async fn run_market_manager(
    cfg: Config,
    http: Arc<KalshiClient>,
    shared: Shared,
    ws_tx: mpsc::Sender<WsMarketCommand>,
    exec_tx: mpsc::Sender<ExecCommand>,
    initial: Vec<ActiveMarketMeta>,
) -> Result<()> {
    // Track one active ticker per series (you can have many series -> many simultaneous markets).
    let mut active_by_series: HashMap<String, ActiveMarketMeta> = HashMap::new();
    for m in initial {
        active_by_series.insert(m.series_ticker.clone(), m);
    }

    let mut interval = time::interval(Duration::from_millis(cfg.market_refresh_ms));

    loop {
        interval.tick().await;

        let now = Utc::now().timestamp();

        // Clone keys so we can mutate map while iterating.
        let series_list: Vec<String> = active_by_series.keys().cloned().collect();

        for series in series_list {
            let Some(cur) = active_by_series.get(&series).cloned() else { continue; };

            // If we don't know close_ts (shouldn't happen), skip.
            if now < cur.close_ts {
                continue;
            }

            tracing::info!(
                "series={} current market {} closed (now={} close_ts={}), rotating...",
                series, cur.market_ticker, now, cur.close_ts
            );

            // Fetch new current market for this series
            let next = fetch_current_market(&http, &series).await?;

            // If ticker didn't change, just refresh times (maybe Kalshi updated close_time)
            if next.market_ticker == cur.market_ticker {
                tracing::info!("series={} active ticker unchanged {}, refreshing times", series, cur.market_ticker);
                seed_shared_times(&shared, &[next.clone()]).await?;
                active_by_series.insert(series.clone(), next);
                continue;
            }

            // 1) Ensure NEW ticker exists in Shared and seed times (so WS snapshot won't be dropped)
            shared.ensure_ticker(&next.market_ticker);
            seed_shared_times(&shared, &[next.clone()]).await?;

            // 2) Tell WS task to update subscriptions:
            //    - add new ticker
            //    - delete old ticker
            let _ = ws_tx.send(WsMarketCommand::UpdateMarkets {
                add: vec![next.market_ticker.clone()],
                remove: vec![cur.market_ticker.clone()],
            }).await;

            // 3) Optional: cancel known resting orders on old ticker
            cancel_known_resting(&exec_tx, &shared, &cur.market_ticker).await;

            // 4) Remove old ticker from Shared to stop engine processing it
            shared.remove_ticker(&cur.market_ticker);

            // 5) Update our map
            active_by_series.insert(series.clone(), next);
        }
    }
}