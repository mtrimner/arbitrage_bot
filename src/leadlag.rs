use std::collections::VecDeque;
use std::io::ErrorKind;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDir {
    Up,
    Down,
}

impl MoveDir {
    pub fn as_str(self) -> &'static str {
        match self {
            MoveDir::Up => "up",
            MoveDir::Down => "down",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpportunitySide {
    BuyYes,
    BuyNo,
}

impl OpportunitySide {
    pub fn as_str(self) -> &'static str {
        match self {
            OpportunitySide::BuyYes => "buy_yes",
            OpportunitySide::BuyNo => "buy_no",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoinbaseTickPoint {
    pub price: f64,
    pub exchange_ts_ms: Option<u64>,
    pub local_ts_ms: u64,
    pub sequence_num: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PostTriggerStats {
    pub best_yes_bid: Option<u8>,
    pub best_no_bid: Option<u8>,
    pub best_yes_ask: Option<u8>,
    pub best_no_ask: Option<u8>,
    pub best_favorable_entry_edge_cents: i16,
    pub best_favorable_exit_edge_cents: i16,

    // Pair-cost tracking for your actual strategy
    pub min_pair_cost_cents: Option<u16>,
    pub min_pair_cost_ts_ms: Option<u64>,
}

impl Default for PostTriggerStats {
    fn default() -> Self {
        Self {
            best_yes_bid: None,
            best_no_bid: None,
            best_yes_ask: None,
            best_no_ask: None,
            best_favorable_entry_edge_cents: 0,
            best_favorable_exit_edge_cents: 0,
            min_pair_cost_cents: None,
            min_pair_cost_ts_ms: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FirstResponse {
    pub response_type: String,
    pub kalshi_response_ts_ms: u64,
    pub lag_ms: u64,
    pub yes_bid_after: Option<u8>,
    pub no_bid_after: Option<u8>,
    pub yes_ask_after: Option<u8>,
    pub no_ask_after: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct CoinbaseMove {
    pub product_id: String,

    pub old_price: f64,
    pub new_price: f64,
    pub delta_usd: f64,
    pub dir: MoveDir,

    pub exchange_ts_ms: Option<u64>,
    pub local_ts_ms: u64,
    pub sequence_num: Option<u64>,

    pub anchor_exchange_ts_ms: Option<u64>,
    pub anchor_local_ts_ms: u64,
    pub anchor_sequence_num: Option<u64>,

    pub window_ms: u64,

    pub kalshi_ticker: String,
    pub kalshi_strike_price: Option<f64>,

    // Trigger-time Kalshi top-of-book snapshot
    pub trigger_yes_bid: Option<u8>,
    pub trigger_no_bid: Option<u8>,
    pub trigger_yes_ask: Option<u8>,
    pub trigger_no_ask: Option<u8>,

    // Hypothetical action at trigger time
    pub opportunity_side: OpportunitySide,
    pub trigger_entry_price_cents: Option<u8>,
    pub trigger_entry_spread_cents: Option<u8>,

    // First response is recorded once, but the event stays open
    pub first_response: Option<FirstResponse>,

    // Rolling best observed post-trigger through full horizon
    pub post: PostTriggerStats,
}

#[derive(Debug, Clone)]
pub struct PendingLeadLag {
    pub move_event: CoinbaseMove,
}

#[derive(Debug, Default)]
pub struct LeadLagTracker {
    pub recent_ticks: VecDeque<CoinbaseTickPoint>,
    pub pending: Option<PendingLeadLag>,
}

pub type SharedLeadLag = Arc<Mutex<LeadLagTracker>>;

pub fn new_shared_leadlag() -> SharedLeadLag {
    Arc::new(Mutex::new(LeadLagTracker::default()))
}

fn fmt_opt_u8(v: Option<u8>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn fmt_opt_u16(v: Option<u16>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn fmt_opt_u64(v: Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn fmt_opt_f64_2(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_default()
}

async fn append_header_if_needed(path: &str) -> Result<()> {
    let header = concat!(
        "kalshi_ticker,kalshi_strike_price,product_id,coinbase_dir,coinbase_old_price,coinbase_new_price,coinbase_delta_usd,",
        "coinbase_anchor_exchange_ts_ms,coinbase_anchor_local_ts_ms,coinbase_anchor_sequence_num,",
        "coinbase_exchange_ts_ms,coinbase_local_ts_ms,coinbase_sequence_num,coinbase_window_ms,",
        "trigger_yes_bid,trigger_yes_ask,trigger_no_bid,trigger_no_ask,",
        "opportunity_side,trigger_entry_price_cents,trigger_entry_spread_cents,",
        "first_response_type,first_response_ts_ms,first_response_lag_ms,",
        "first_yes_bid_after,first_no_bid_after,first_yes_ask_after,first_no_ask_after,",
        "best_yes_bid_post,best_yes_ask_post,best_no_bid_post,best_no_ask_post,",
        "best_favorable_entry_edge_cents,best_favorable_exit_edge_cents,",
        "min_pair_cost_cents,min_pair_cost_ts_ms\n"
    );

    let needs_header = match tokio::fs::metadata(path).await {
        Ok(m) => {
            if m.len() == 0 {
                true
            } else {
                let existing = tokio::fs::read_to_string(path)
                    .await
                    .with_context(|| format!("read existing leadlag file {}", path))?;
                let first_line = existing.lines().next().unwrap_or_default();
                let expected = header.trim_end();
                let found = first_line.trim_end();
                if found != expected {
                    anyhow::bail!(
                        "leadlag csv header mismatch in {}. Expected current schema with kalshi_strike_price; rotate or clear the file before logging again",
                        path
                    );
                }
                false
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => true,
        Err(e) => return Err(e).context("metadata(leadlag file)"),
    };

    if !needs_header {
        return Ok(());
    }

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open leadlag file {}", path))?;

    f.write_all(header.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}

pub async fn append_leadlag_row(path: &str, ev: &CoinbaseMove) -> Result<()> {
    append_header_if_needed(path).await?;

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open leadlag file {}", path))?;

    let line = [
        ev.kalshi_ticker.clone(),
        fmt_opt_f64_2(ev.kalshi_strike_price),
        ev.product_id.clone(),
        ev.dir.as_str().to_string(),
        format!("{:.2}", ev.old_price),
        format!("{:.2}", ev.new_price),
        format!("{:.2}", ev.delta_usd),
        fmt_opt_u64(ev.anchor_exchange_ts_ms),
        ev.anchor_local_ts_ms.to_string(),
        fmt_opt_u64(ev.anchor_sequence_num),
        fmt_opt_u64(ev.exchange_ts_ms),
        ev.local_ts_ms.to_string(),
        fmt_opt_u64(ev.sequence_num),
        ev.window_ms.to_string(),
        fmt_opt_u8(ev.trigger_yes_bid),
        fmt_opt_u8(ev.trigger_yes_ask),
        fmt_opt_u8(ev.trigger_no_bid),
        fmt_opt_u8(ev.trigger_no_ask),
        ev.opportunity_side.as_str().to_string(),
        fmt_opt_u8(ev.trigger_entry_price_cents),
        fmt_opt_u8(ev.trigger_entry_spread_cents),
        ev.first_response
            .as_ref()
            .map(|x| x.response_type.clone())
            .unwrap_or_default(),
        ev.first_response
            .as_ref()
            .map(|x| x.kalshi_response_ts_ms.to_string())
            .unwrap_or_default(),
        ev.first_response
            .as_ref()
            .map(|x| x.lag_ms.to_string())
            .unwrap_or_default(),
        fmt_opt_u8(ev.first_response.as_ref().and_then(|x| x.yes_bid_after)),
        fmt_opt_u8(ev.first_response.as_ref().and_then(|x| x.no_bid_after)),
        fmt_opt_u8(ev.first_response.as_ref().and_then(|x| x.yes_ask_after)),
        fmt_opt_u8(ev.first_response.as_ref().and_then(|x| x.no_ask_after)),
        fmt_opt_u8(ev.post.best_yes_bid),
        fmt_opt_u8(ev.post.best_yes_ask),
        fmt_opt_u8(ev.post.best_no_bid),
        fmt_opt_u8(ev.post.best_no_ask),
        ev.post.best_favorable_entry_edge_cents.to_string(),
        ev.post.best_favorable_exit_edge_cents.to_string(),
        fmt_opt_u16(ev.post.min_pair_cost_cents),
        fmt_opt_u64(ev.post.min_pair_cost_ts_ms),
    ]
    .join(",")
        + "\n";

    f.write_all(line.as_bytes()).await?;
    f.flush().await?;

    info!(
        target: "leadlag",
        kalshi_ticker = %ev.kalshi_ticker,
        kalshi_strike_price = ?ev.kalshi_strike_price,
        dir = %ev.dir.as_str(),
        trigger_side = %ev.opportunity_side.as_str(),
        trigger_entry_price_cents = ?ev.trigger_entry_price_cents,
        first_response_lag_ms = ?ev.first_response.as_ref().map(|x| x.lag_ms),
        best_favorable_entry_edge_cents = ev.post.best_favorable_entry_edge_cents,
        best_favorable_exit_edge_cents = ev.post.best_favorable_exit_edge_cents,
        min_pair_cost_cents = ?ev.post.min_pair_cost_cents,
        "coinbase->kalshi full-horizon opportunity event"
    );

    Ok(())
}
