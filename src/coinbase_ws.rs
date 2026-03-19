use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cbadv::{
    WebSocketClientBuilder,
    models::websocket::{Channel, EndpointStream},
    types::CbResult,
};
use chrono::DateTime;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::leadlag::{
    CoinbaseMove, CoinbaseTickPoint, MoveDir, OpportunitySide, PendingLeadLag, PostTriggerStats,
    SharedLeadLag,
};
use crate::state::Shared;
use crate::state::coinbase::CoinbaseBookSide;
use crate::types::Side;

#[derive(Debug, Clone)]
pub struct CoinbasePrice {
    pub product_id: String,
    pub price: f64,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub ts_ms: u64,
    pub exchange_ts_ms: Option<u64>,
    pub sequence_num: Option<u64>,
}

impl CoinbasePrice {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64
    }
}

#[derive(Debug, Deserialize)]
struct CoinbaseHeader {
    channel: String,
}

#[derive(Debug, Deserialize)]
struct CoinbaseTickerEnvelope {
    channel: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    sequence_num: Option<u64>,
    #[serde(default)]
    events: Vec<CoinbaseTickerEvent>,
}

#[derive(Debug, Default, Deserialize)]
struct CoinbaseTickerEvent {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    tickers: Vec<CoinbaseTicker>,
}

#[derive(Debug, Deserialize)]
struct CoinbaseTicker {
    product_id: String,
    price: String,
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
    #[serde(default)]
    best_bid_quantity: Option<String>,
    #[serde(default)]
    best_ask_quantity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CoinbaseL2Envelope {
    channel: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    sequence_num: Option<u64>,
    #[serde(default)]
    events: Vec<CoinbaseL2Event>,
}

#[derive(Debug, Default, Deserialize)]
struct CoinbaseL2Event {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(default)]
    updates: Vec<CoinbaseL2Update>,
}

#[derive(Debug, Deserialize)]
struct CoinbaseL2Update {
    side: String,
    price_level: String,
    new_quantity: String,
}

#[derive(Debug, Deserialize)]
struct CoinbaseHeartbeatEnvelope {
    channel: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    sequence_num: Option<u64>,
    #[serde(default)]
    events: Vec<CoinbaseHeartbeatEvent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum JsonU64 {
    String(String),
    Number(u64),
}

impl JsonU64 {
    fn to_u64(&self) -> Option<u64> {
        match self {
            JsonU64::String(s) => s.parse::<u64>().ok(),
            JsonU64::Number(v) => Some(*v),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct CoinbaseHeartbeatEvent {
    #[serde(default)]
    heartbeat_counter: Option<JsonU64>,
}

fn parse_f64_field(raw: &str, field: &str, product_id: &str) -> Option<f64> {
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() => Some(v),
        Ok(_) => {
            warn!(%product_id, %field, %raw, "coinbase ws non-finite numeric field");
            None
        }
        Err(e) => {
            warn!(%product_id, %field, %raw, err = %e, "coinbase ws numeric parse failed");
            None
        }
    }
}

fn parse_optional_f64_field(raw: Option<&str>, field: &str, product_id: &str) -> Option<f64> {
    raw.and_then(|v| parse_f64_field(v, field, product_id))
}

fn parse_exchange_ts_ms(raw: Option<&str>) -> Option<u64> {
    let ts = raw?;
    let dt = DateTime::parse_from_rfc3339(ts).ok()?;
    let ms = dt.timestamp_millis();
    if ms < 0 { None } else { Some(ms as u64) }
}

fn same_opt_f64(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.to_bits() == y.to_bits(),
        (None, None) => true,
        _ => false,
    }
}

fn should_publish(cur: &CoinbasePrice, next: &CoinbasePrice) -> bool {
    cur.product_id != next.product_id
        || cur.price.to_bits() != next.price.to_bits()
        || !same_opt_f64(cur.best_bid, next.best_bid)
        || !same_opt_f64(cur.best_ask, next.best_ask)
        || cur.sequence_num != next.sequence_num
}

async fn handle_ticker_message(
    text: &str,
    expected_product_id: &str,
    tx: &watch::Sender<CoinbasePrice>,
    cfg: &Config,
    shared: &Shared,
) {
    let envelope: CoinbaseTickerEnvelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(err = %e, raw = %text, "coinbase ws ticker json parse failed");
            return;
        }
    };

    if envelope.channel != "ticker" {
        return;
    }

    let now_ms = CoinbasePrice::now_ms();
    let exchange_ts_ms = parse_exchange_ts_ms(envelope.timestamp.as_deref());
    let sequence_num = envelope.sequence_num;

    for event in envelope.events {
        if let Some(kind) = &event.r#type {
            if kind != "update" && kind != "snapshot" {
                debug!(event_type = %kind, "coinbase ws ignoring unexpected ticker event type");
            }
        }

        for ticker in event.tickers {
            if ticker.product_id != expected_product_id {
                continue;
            }

            let Some(price) = parse_f64_field(&ticker.price, "price", &ticker.product_id) else {
                continue;
            };

            let best_bid = parse_optional_f64_field(
                ticker.best_bid.as_deref(),
                "best_bid",
                &ticker.product_id,
            );
            let best_ask = parse_optional_f64_field(
                ticker.best_ask.as_deref(),
                "best_ask",
                &ticker.product_id,
            );
            let best_bid_qty = parse_optional_f64_field(
                ticker.best_bid_quantity.as_deref(),
                "best_bid_quantity",
                &ticker.product_id,
            );
            let best_ask_qty = parse_optional_f64_field(
                ticker.best_ask_quantity.as_deref(),
                "best_ask_quantity",
                &ticker.product_id,
            );

            {
                let mut coinbase = shared.coinbase.write().await;
                let changed = coinbase.apply_ticker(
                    cfg,
                    now_ms,
                    price,
                    best_bid,
                    best_ask,
                    best_bid_qty,
                    best_ask_qty,
                );
                drop(coinbase);
                if changed {
                    shared.touch_all();
                }
            }

            let next = CoinbasePrice {
                product_id: ticker.product_id,
                price,
                best_bid,
                best_ask,
                ts_ms: now_ms,
                exchange_ts_ms,
                sequence_num,
            };

            let publish = {
                let cur = tx.borrow();
                should_publish(&cur, &next)
            };

            if publish {
                let _ = tx.send(next);
            }
        }
    }
}

async fn handle_l2_message(text: &str, expected_product_id: &str, cfg: &Config, shared: &Shared) {
    let envelope: CoinbaseL2Envelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(err = %e, raw = %text, "coinbase ws l2 json parse failed");
            return;
        }
    };

    if envelope.channel != "l2_data" && envelope.channel != "level2" {
        return;
    }

    let now_ms = CoinbasePrice::now_ms();

    for event in envelope.events {
        let event_type = event.r#type.clone();
        let product_id = event.product_id.as_deref().unwrap_or(expected_product_id);
        if product_id != expected_product_id {
            continue;
        }

        let mut parsed = Vec::with_capacity(event.updates.len());
        for update in event.updates {
            let Some(price) = parse_f64_field(&update.price_level, "price_level", product_id)
            else {
                continue;
            };
            let Some(qty) = parse_f64_field(&update.new_quantity, "new_quantity", product_id)
            else {
                continue;
            };
            let side = match update.side.to_ascii_lowercase().as_str() {
                "bid" => CoinbaseBookSide::Bid,
                "offer" | "ask" => CoinbaseBookSide::Ask,
                other => {
                    debug!(side = %other, "coinbase ws ignoring unknown l2 side");
                    continue;
                }
            };
            parsed.push((side, price, qty));
        }

        if parsed.is_empty() {
            continue;
        }

        let changed = {
            let mut coinbase = shared.coinbase.write().await;
            match event_type.as_deref() {
                Some("snapshot") => coinbase.apply_level2_snapshot(cfg, now_ms, &parsed),
                _ => coinbase.apply_level2_update(cfg, now_ms, &parsed),
            }
        };

        if changed {
            shared.touch_all();
        }
    }
}

async fn handle_heartbeat_message(text: &str, shared: &Shared) {
    let envelope: CoinbaseHeartbeatEnvelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(err = %e, raw = %text, "coinbase ws heartbeat json parse failed");
            return;
        }
    };

    if envelope.channel != "heartbeats" {
        return;
    }

    let ts_ms = CoinbasePrice::now_ms();
    let heartbeat_counter = envelope
        .events
        .first()
        .and_then(|e| e.heartbeat_counter.as_ref())
        .and_then(JsonU64::to_u64);

    let mut coinbase = shared.coinbase.write().await;
    coinbase.record_heartbeat(ts_ms, heartbeat_counter);
}

async fn handle_text_message(
    text: &str,
    expected_product_id: &str,
    tx: &watch::Sender<CoinbasePrice>,
    cfg: &Config,
    shared: &Shared,
) {
    let header: CoinbaseHeader = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(err = %e, raw = %text, "coinbase ws raw json parse failed");
            return;
        }
    };

    match header.channel.as_str() {
        "ticker" => handle_ticker_message(text, expected_product_id, tx, cfg, shared).await,
        "l2_data" | "level2" => handle_l2_message(text, expected_product_id, cfg, shared).await,
        "heartbeats" => handle_heartbeat_message(text, shared).await,
        other => debug!(channel = %other, "coinbase ws ignoring channel"),
    }
}

pub fn spawn_coinbase_ticker(
    product_id: String,
    cfg: Config,
    shared: Shared,
) -> watch::Receiver<CoinbasePrice> {
    let (tx, rx) = watch::channel(CoinbasePrice {
        product_id: product_id.clone(),
        price: f64::NAN,
        best_bid: None,
        best_ask: None,
        ts_ms: 0,
        exchange_ts_ms: None,
        sequence_num: None,
    });

    tokio::spawn(async move {
        loop {
            match run_coinbase_ticker_once(
                product_id.clone(),
                tx.clone(),
                cfg.clone(),
                shared.clone(),
            )
            .await
            {
                Ok(()) => warn!("coinbase ws exited; restarting"),
                Err(e) => warn!(err = %format!("{e:?}"), "coinbase ws error; restarting"),
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    });

    rx
}

async fn run_coinbase_ticker_once(
    product_id: String,
    tx: watch::Sender<CoinbasePrice>,
    cfg: Config,
    shared: Shared,
) -> CbResult<()> {
    let mut ws_client = WebSocketClientBuilder::new()
        .use_public(true)
        .auto_reconnect(false)
        .build()?;

    let readers = ws_client.connect().await?;
    let products = vec![product_id.clone()];
    let empty: Vec<String> = Vec::new();

    ws_client.subscribe(&Channel::Ticker, &products).await?;
    ws_client.subscribe(&Channel::Level2, &products).await?;
    ws_client.subscribe(&Channel::Heartbeats, &empty).await?;

    info!(
        product_id = %product_id,
        channels = ?["ticker", "level2", "heartbeats"],
        "coinbase ws connected and subscribed"
    );

    let mut stream: EndpointStream = readers.into();

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(ws_msg) => {
                if ws_msg.is_close() {
                    warn!("coinbase ws close frame received");
                    break;
                }

                if let Ok(text) = ws_msg.to_text() {
                    handle_text_message(text, &product_id, &tx, &cfg, &shared).await;
                }
            }
            Err(e) => {
                warn!(err = %e, "coinbase ws read error");
                break;
            }
        }
    }

    Ok(())
}

pub fn spawn_coinbase_logger(mut rx: watch::Receiver<CoinbasePrice>, min_delta_usd: f64) {
    tokio::spawn(async move {
        let mut last = None::<CoinbasePrice>;

        loop {
            if rx.changed().await.is_err() {
                break;
            }

            let cur = rx.borrow().clone();
            let should_log = match &last {
                None => true,
                Some(prev) => (cur.price - prev.price).abs() >= min_delta_usd,
            };

            if should_log {
                let exchange_to_local_ms =
                    cur.exchange_ts_ms.map(|ex| cur.ts_ms.saturating_sub(ex));

                debug!(
                    target: "coinbase",
                    product_id = %cur.product_id,
                    price = cur.price,
                    best_bid = ?cur.best_bid,
                    best_ask = ?cur.best_ask,
                    ts_ms = cur.ts_ms,
                    exchange_ts_ms = ?cur.exchange_ts_ms,
                    sequence_num = ?cur.sequence_num,
                    exchange_to_local_ms = ?exchange_to_local_ms,
                    "coinbase ticker"
                );

                last = Some(cur);
            }
        }
    });
}

pub async fn spawn_coinbase_move_detector(
    mut rx: watch::Receiver<CoinbasePrice>,
    cfg: Config,
    shared: Shared,
    leadlag: SharedLeadLag,
) {
    loop {
        if rx.changed().await.is_err() {
            break;
        }

        let cur = rx.borrow().clone();
        if !cur.price.is_finite() {
            continue;
        }

        let (anchor, delta) = {
            let mut g = leadlag.lock().await;

            if g.pending.is_some() {
                continue;
            }

            g.recent_ticks.push_back(CoinbaseTickPoint {
                price: cur.price,
                exchange_ts_ms: cur.exchange_ts_ms,
                local_ts_ms: cur.ts_ms,
                sequence_num: cur.sequence_num,
            });

            let cutoff_ms = cur.ts_ms.saturating_sub(cfg.coinbase_leadlag_window_ms);
            while let Some(front) = g.recent_ticks.front() {
                if front.local_ts_ms < cutoff_ms {
                    g.recent_ticks.pop_front();
                } else {
                    break;
                }
            }

            let Some(anchor) = g.recent_ticks.front().cloned() else {
                continue;
            };

            let delta = cur.price - anchor.price;
            if delta.abs() < cfg.coinbase_leadlag_min_move_usd {
                continue;
            }

            (anchor, delta)
        };

        let kalshi_ticker = match shared.tickers.iter().next() {
            Some(item) => item.key().clone(),
            None => continue,
        };

        let Some(ts) = shared.tickers.get(&kalshi_ticker) else {
            continue;
        };

        let m = ts.mkt.read().await;
        let kalshi_strike_price = m.strike_price;
        let trigger_yes_bid = m.book.best_yes_bid();
        let trigger_no_bid = m.book.best_no_bid();
        let trigger_yes_ask = m.book.best_yes_ask();
        let trigger_no_ask = m.book.best_no_ask();

        let dir = if delta > 0.0 {
            MoveDir::Up
        } else {
            MoveDir::Down
        };
        let opportunity_side = match dir {
            MoveDir::Up => OpportunitySide::BuyYes,
            MoveDir::Down => OpportunitySide::BuyNo,
        };

        let trigger_entry_price_cents = match opportunity_side {
            OpportunitySide::BuyYes => trigger_yes_ask,
            OpportunitySide::BuyNo => trigger_no_ask,
        };

        let trigger_entry_spread_cents = match opportunity_side {
            OpportunitySide::BuyYes => m.book.spread_cents(Side::Yes),
            OpportunitySide::BuyNo => m.book.spread_cents(Side::No),
        };

        drop(m);

        // Keep the lead/lag mutex out of the market-lock path. The WS task updates
        // lead/lag while holding the market write lock, so holding both here can
        // deadlock the bot.
        let mut g = leadlag.lock().await;
        if g.pending.is_some() {
            continue;
        }
        g.pending = Some(PendingLeadLag {
            move_event: CoinbaseMove {
                product_id: cur.product_id.clone(),
                old_price: anchor.price,
                new_price: cur.price,
                delta_usd: delta,
                dir,
                exchange_ts_ms: cur.exchange_ts_ms,
                local_ts_ms: cur.ts_ms,
                sequence_num: cur.sequence_num,
                anchor_exchange_ts_ms: anchor.exchange_ts_ms,
                anchor_local_ts_ms: anchor.local_ts_ms,
                anchor_sequence_num: anchor.sequence_num,
                window_ms: cfg.coinbase_leadlag_window_ms,
                kalshi_ticker,
                kalshi_strike_price,
                trigger_yes_bid,
                trigger_no_bid,
                trigger_yes_ask,
                trigger_no_ask,
                opportunity_side,
                trigger_entry_price_cents,
                trigger_entry_spread_cents,
                first_response: None,
                post: PostTriggerStats::default(),
            },
        });
    }
}
