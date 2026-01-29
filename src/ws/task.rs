use anyhow::Result;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc;

use kalshi_rs::{KalshiClient, KalshiWebsocketClient};
use kalshi_rs::websocket::models::{
    KalshiSocketMessage, OrderbookSnapshot, OrderbookDelta, TradeUpdate, UserFill,
    SubscribedResponse, OkResponse, ErrorResponse,
};

use crate::config::Config;
use crate::state::Shared;
use crate::types::{Side, TradeLite, WsMarketCommand};

fn parse_side(s: &str) -> Option<Side> {
    match s.to_ascii_lowercase().as_str() {
        "yes" => Some(Side::Yes),
        "no" => Some(Side::No),
        _ => None,
    }
}

pub async fn run_ws(
    mut ws: KalshiWebsocketClient,
    _http: Arc<KalshiClient>,
    cfg: Config,
    shared: Shared,
    initial_tickers: Vec<String>,
    mut ctl_rx: mpsc::Receiver<WsMarketCommand>,
) -> Result<()> {
    // Track our current subscribed markets locally so reconnects resubscribe correctly.
    let mut markets: HashSet<String> = initial_tickers.into_iter().collect();

    // channel -> sid
    let mut sids: HashMap<String, u64> = HashMap::new();

    // Commands that arrive before we have sids can be queued.
    let mut pending: Vec<WsMarketCommand> = Vec::new();

    // The channels we care about for trading.
    let channels: Vec<&str> = vec!["orderbook_delta", "trade", "fill"];

    loop {
        // Drain any queued control commands before connecting (keeps markets set up to date).
        while let Ok(cmd) = ctl_rx.try_recv() {
            apply_ctl_local(&mut markets, &cmd);
            pending.push(cmd);
        }

        if let Err(e) = ws.connect().await {
            warn!("ws connect failed {e:?}");
            sleep(Duration::from_millis(500)).await;
            continue;
        }

        // Reset sids for this connection (new connection => new subscription ids).
        sids.clear();

        let trefs: Vec<String> = markets.iter().cloned().collect();
        let trefs_ref: Vec<&str> = trefs.iter().map(|s| s.as_str()).collect();

        if let Err(e) = ws.subscribe(channels.clone(), trefs_ref).await {
            warn!("ws subscribe failed: {e:?}");
            sleep(Duration::from_millis(500)).await;
            continue;
        }

        info!("ws connected+subscribed to {} tickers", markets.len());

        // Inner loop: handle WS messages and control commands concurrently.
        loop {
            tokio::select! {
                msg = ws.next_message() => {
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("ws read error: {e:?} (reconnect)");
                            break;
                        }
                    };

                    match msg {
                        KalshiSocketMessage::SubscribedResponse(sr) => {
                            handle_subscribed(&mut sids, sr);
                            // If we now have all sids, apply any pending market updates.
                            if has_all_sids(&sids) && !pending.is_empty() {
                                let pend = std::mem::take(&mut pending);
                                for cmd in pend {
                                    if let Err(e) = apply_update_subscription(&ws, &sids, &cmd).await {
                                        warn!("apply pending update failed: {e:?}");
                                    }
                                }
                            }
                        }
                        KalshiSocketMessage::OkResponse(ok) => {
                            handle_ok(ok);
                        }
                        KalshiSocketMessage::ErrorResponse(err) => {
                            handle_err(err);
                        }

                        KalshiSocketMessage::OrderbookSnapshot(snap) => {
                            handle_snapshot(&cfg, &shared, snap).await?;
                        }
                        KalshiSocketMessage::OrderbookDelta(delta) => {
                            let ok = handle_delta(&cfg, &shared, delta).await?;
                            if !ok {
                                warn!("orderbook seq gap detected; reconnecting");
                                break;
                            }
                        }
                        KalshiSocketMessage::TradeUpdate(tu) => {
                            handle_trade(&cfg, &shared, tu).await?;
                        }
                        KalshiSocketMessage::UserFill(uf) => {
                            handle_fill(&shared, uf).await?;
                        }
                        _ => {}
                    }
                }

                cmd = ctl_rx.recv() => {
                    let Some(cmd) = cmd else { return Ok(()); };

                    // Always update local view (for reconnect correctness)
                    apply_ctl_local(&mut markets, &cmd);

                    // If we don't have sids yet, queue it.
                    if !has_all_sids(&sids) {
                        pending.push(cmd);
                        continue;
                    }

                    // Apply update_subscription calls
                    if let Err(e) = apply_update_subscription(&ws, &sids, &cmd).await {
                        warn!("ws update_subscription failed: {e:?}");
                    }
                }
            }
        }

        sleep(Duration::from_millis(250)).await;
    }
}

fn handle_subscribed(sids: &mut HashMap<String,u64>, sr: SubscribedResponse) {
    let ch = sr.msg.channel;
    let sid = sr.msg.sid as u64;
    info!("subscribed channel={} sid={}", ch, sid);
    sids.insert(ch, sid);
}

fn handle_ok(ok: OkResponse) {
    // Often returned by update_subscription; contains sid + affected market_tickers.
    info!("ok response id={} sid={} markets={:?}", ok.id, ok.sid, ok.msg.market_tickers);
}

fn handle_err(err: ErrorResponse) {
    warn!("ws error id={} code={} msg={}", err.id, err.msg.code, err.msg.msg);
}

fn has_all_sids(sids: &HashMap<String,u64>) -> bool {
    // We only update subscriptions for these three channels
    sids.contains_key("orderbook_delta") && sids.contains_key("trade") && sids.contains_key("fill")
}

fn apply_ctl_local(markets: &mut HashSet<String>, cmd: &WsMarketCommand) {
    match cmd {
        WsMarketCommand::UpdateMarkets { add, remove } => {
            for t in add { markets.insert(t.clone()); }
            for t in remove { markets.remove(t); }
        }
    }
}

/// Apply add/delete markets on each channel sid.
/// We do ADD first, then DELETE, so we minimize “no subscription” gaps.
async fn apply_update_subscription(
    ws: &KalshiWebsocketClient,
    sids: &HashMap<String,u64>,
    cmd: &WsMarketCommand,
) -> Result<()> {
    let (add, remove) = match cmd {
        WsMarketCommand::UpdateMarkets { add, remove } => (add, remove),
    };

    let channels = ["orderbook_delta", "trade", "fill"];

    for ch in channels {
        let Some(&sid) = sids.get(ch) else { continue; };

        if !add.is_empty() {
            let add_refs: Vec<&str> = add.iter().map(|s| s.as_str()).collect();
            ws.add_markets(vec![sid], add_refs).await?;
        }
        if !remove.is_empty() {
            let rem_refs: Vec<&str> = remove.iter().map(|s| s.as_str()).collect();
            ws.del_markets(vec![sid], rem_refs).await?;
        }
    }

    Ok(())
}

// --- your existing handlers below (unchanged except signature tweaks if needed) ---

async fn handle_snapshot(_cfg: &Config, shared: &Shared, snap: OrderbookSnapshot) -> Result<()> {
    let seq = snap.seq;
    let m = snap.msg;
    let ticker = m.market_ticker.clone();
    let yes = m.yes.unwrap_or_default();
    let no = m.no.unwrap_or_default();

    let ts = shared.ensure_ticker(&ticker);
    let mut g = ts.mkt.write().await;
    g.book.reset(seq, &yes, &no);

    ts.mark_dirty();
    shared.notify.notify_one();
    Ok(())
}

async fn handle_delta(_cfg: &Config, shared: &Shared, delta: OrderbookDelta) -> Result<bool> {
    let seq = delta.seq;
    let m = delta.msg;
    let ticker = m.market_ticker.clone();
    let Some(side) = parse_side(&m.side) else { return Ok(true); };

    let ts = shared.ensure_ticker(&ticker);
    let mut g = ts.mkt.write().await;
    let ok = g.book.apply_delta(seq, side, m.price, m.delta);

    ts.mark_dirty();
    shared.notify.notify_one();
    Ok(ok)
}

async fn handle_trade(_cfg: &Config, shared: &Shared, tu: TradeUpdate) -> Result<()> {
    let m = tu.msg;
    let ticker = m.market_ticker.clone();
    let Some(taker_side) = parse_side(&m.taker_side) else { return Ok(()); };

    let ts = shared.ensure_ticker(&ticker);
    let mut g = ts.mkt.write().await;
    g.push_trade(TradeLite {
        ts: m.ts,
        taker_side,
        count: m.count,
        yes_price: m.yes_price,
        no_price: m.no_price,
    });

    ts.mark_dirty();
    shared.notify.notify_one();
    Ok(())
}

async fn handle_fill(shared: &Shared, uf: UserFill) -> Result<()> {
    let m = uf.msg;
    let ticker = m.market_ticker.clone();

    let Some(purchased) = parse_side(&m.purchased_side) else { return Ok(()); };
    let qty = m.count.max(0) as i64;

    let price = match purchased {
        Side::Yes => m.yes_price,
        Side::No => 100u8.saturating_sub(m.yes_price),
    };

    if let Some(ts) = shared.tickers.get(&ticker) {
        let mut g = ts.mkt.write().await;
        g.pos.apply_fill(purchased, price, qty);
        ts.mark_dirty();
        shared.notify.notify_one();
    }
    Ok(())
}
