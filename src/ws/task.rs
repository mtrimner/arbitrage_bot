use anyhow::Result;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};
use uuid::Uuid;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc;

use kalshi_rs::websocket::models::{
    ErrorResponse, KalshiSocketMessage, OkResponse, OrderbookDelta, OrderbookSnapshot,
    SubscribedResponse, TradeUpdate, UserFill,
};
use kalshi_rs::{KalshiClient, KalshiWebsocketClient};

use crate::config::Config;
use crate::leadlag::{FirstResponse, OpportunitySide};
use crate::state::Shared;
use crate::state::book::Book;
use crate::types::{Side, WsMarketCommand};
use std::time::{SystemTime, UNIX_EPOCH};

const WS_CHANNELS: [&str; 3] = ["orderbook_delta", "trade", "fill"];

pub async fn run_ws(
    ws: KalshiWebsocketClient,
    _http: Arc<KalshiClient>,
    cfg: Config,
    shared: Shared,
    leadlag: crate::leadlag::SharedLeadLag,
    initial_tickers: Vec<String>,
    mut ctl_rx: mpsc::Receiver<WsMarketCommand>,
) -> Result<()> {
    // Track our current subscribed markets locally so reconnects resubscribe correctly.
    let mut markets: HashSet<String> = initial_tickers.into_iter().collect();

    // channel -> sid
    let mut sids: HashMap<String, u64> = HashMap::new();

    // Commands that arrive before we have sids can be queued.
    let mut pending: Vec<WsMarketCommand> = Vec::new();

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

        if let Err(e) = ws.subscribe(WS_CHANNELS.to_vec(), trefs_ref).await {
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
                            handle_snapshot(&shared, snap).await?;
                        }
                        KalshiSocketMessage::OrderbookDelta(delta) => {
                            let ok = handle_delta(&cfg, &shared, &leadlag, delta).await?;
                            if !ok {
                                warn!("orderbook seq gap detected; reconnecting");
                                break;
                            }
                        }
                        KalshiSocketMessage::TradeUpdate(tu) => {
                            // println!("TradeUpdate: {:#?}", tu);
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

fn handle_subscribed(sids: &mut HashMap<String, u64>, sr: SubscribedResponse) {
    let ch = sr.msg.channel;
    let sid = sr.msg.sid as u64;
    info!("subscribed channel={} sid={}", ch, sid);
    sids.insert(ch, sid);
}

fn handle_ok(ok: OkResponse) {
    // Often returned by update_subscription; contains sid + affected market_tickers.
    info!(
        "ok response id={} sid={} markets={:?}",
        ok.id, ok.sid, ok.msg.market_tickers
    );
}

fn handle_err(err: ErrorResponse) {
    warn!(
        "ws error id={} code={} msg={}",
        err.id, err.msg.code, err.msg.msg
    );
}

fn has_all_sids(sids: &HashMap<String, u64>) -> bool {
    // We only update subscriptions for these three channels
    WS_CHANNELS.iter().all(|ch| sids.contains_key(*ch))
}

fn apply_ctl_local(markets: &mut HashSet<String>, cmd: &WsMarketCommand) {
    match cmd {
        WsMarketCommand::UpdateMarkets { add, remove } => {
            for t in add {
                markets.insert(t.clone());
            }
            for t in remove {
                markets.remove(t);
            }
        }
    }
}

/// Apply add/delete markets on each channel sid.
/// We do ADD first, then DELETE, so we minimize “no subscription” gaps.
async fn apply_update_subscription(
    ws: &KalshiWebsocketClient,
    sids: &HashMap<String, u64>,
    cmd: &WsMarketCommand,
) -> Result<()> {
    let (add, remove) = match cmd {
        WsMarketCommand::UpdateMarkets { add, remove } => (add, remove),
    };

    for ch in WS_CHANNELS {
        let Some(&sid) = sids.get(ch) else {
            continue;
        };

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

async fn handle_snapshot(shared: &Shared, snap: OrderbookSnapshot) -> Result<()> {
    let seq = snap.seq;
    let m = snap.msg;
    let ticker = m.market_ticker.clone();
    let yes = m.yes.unwrap_or_default();
    let no = m.no.unwrap_or_default();

    let Some(ts) = shared.tickers.get(&ticker) else {
        return Ok(());
    };
    let mut g = ts.mkt.write().await;
    g.book.reset(seq, &yes, &no);

    ts.touch(&shared);
    Ok(())
}

async fn handle_delta(
    cfg: &Config,
    shared: &Shared,
    leadlag: &crate::leadlag::SharedLeadLag,
    delta: OrderbookDelta,
) -> Result<bool> {
    let seq = delta.seq;
    let m = delta.msg;
    let ticker = m.market_ticker.clone();
    let Some(side) = m.side.parse::<Side>().ok() else {
        return Ok(true);
    };

    let ts = shared.ensure_ticker(&ticker);
    let mut g = ts.mkt.write().await;
    let ok = g.book.apply_delta(seq, side, m.price, m.delta);
    if ok {
        if cfg.exec_mode.is_paper() {
            crate::exec::paper::paper_on_delta_queue(&mut g, side, m.price, m.delta);
        }

        if cfg.coinbase_leadlag_enabled {
            maybe_record_leadlag(cfg, leadlag, &ticker, &g.book).await?;
        }
    }
    ts.touch(&shared);
    Ok(ok)
}

async fn handle_trade(cfg: &Config, shared: &Shared, tu: TradeUpdate) -> Result<()> {
    let m = tu.msg;
    let ticker = m.market_ticker.clone();
    let Some(taker_side) = m.taker_side.parse::<Side>().ok() else {
        return Ok(());
    };

    let ts = shared.ensure_ticker(&ticker);
    let mut g = ts.mkt.write().await;

    if cfg.exec_mode.is_paper() {
        crate::exec::paper::paper_on_trade_fill(
            &ticker,
            &mut g,
            taker_side,
            m.yes_price,
            m.no_price,
            m.count,
        );
    }

    ts.touch(&shared);
    Ok(())
}

async fn handle_fill(shared: &Shared, uf: UserFill) -> Result<()> {
    let m = uf.msg;
    let ticker = m.market_ticker.clone();

    let Some(purchased) = m.purchased_side.parse::<Side>().ok() else {
        return Ok(());
    };

    let fill_qty = m.count.max(0) as i64;
    if fill_qty == 0 {
        return Ok(());
    }

    let price = match purchased {
        Side::Yes => m.yes_price,
        Side::No => 100u8.saturating_sub(m.yes_price),
    };

    if let Some(ts) = shared.tickers.get(&ticker) {
        let mut g = ts.mkt.write().await;

        // Update position.
        g.pos.apply_fill(purchased, price, fill_qty);
        if let Ok(client_id) = Uuid::parse_str(&m.client_order_id) {
            // Make sure order_id mapping exists even if Rest ack is late
            g.orders.link_order_id_if_missing(client_id, &m.order_id);

            let fully_filled = g.orders.record_fill_by_order(&m.order_id, fill_qty as u64);

            if matches!(fully_filled, Some(true)) {
                if let Some(h) = g.resting_hint(purchased).as_ref() {
                    if h.order_id.as_deref() == Some(&m.order_id.as_str()) {
                        *g.resting_hint_mut(purchased) = None;
                    }
                }
            }
        } else {
            // Fallback: apply by order_id (works only if by_order mapping exists)
            let fully_filled = g.orders.record_fill_by_order(&m.order_id, fill_qty as u64);

            if matches!(fully_filled, Some(true)) {
                if let Some(h) = g.resting_hint(purchased).as_ref() {
                    if h.order_id.as_deref() == Some(m.order_id.as_str()) {
                        *g.resting_hint_mut(purchased) = None;
                    }
                }
            }
        }

        ts.touch(&shared);
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn favorable_entry_edge_cents(
    side: OpportunitySide,
    trigger_entry_price_cents: Option<u8>,
    yes_ask_now: Option<u8>,
    no_ask_now: Option<u8>,
) -> i16 {
    let Some(entry) = trigger_entry_price_cents else {
        return 0;
    };

    match side {
        OpportunitySide::BuyYes => {
            let Some(now_ask) = yes_ask_now else {
                return 0;
            };
            now_ask as i16 - entry as i16
        }
        OpportunitySide::BuyNo => {
            let Some(now_ask) = no_ask_now else {
                return 0;
            };
            now_ask as i16 - entry as i16
        }
    }
}

fn favorable_exit_edge_cents(
    side: OpportunitySide,
    trigger_entry_price_cents: Option<u8>,
    yes_bid_now: Option<u8>,
    no_bid_now: Option<u8>,
) -> i16 {
    let Some(entry) = trigger_entry_price_cents else {
        return 0;
    };

    match side {
        OpportunitySide::BuyYes => {
            let Some(now_bid) = yes_bid_now else {
                return 0;
            };
            now_bid as i16 - entry as i16
        }
        OpportunitySide::BuyNo => {
            let Some(now_bid) = no_bid_now else {
                return 0;
            };
            now_bid as i16 - entry as i16
        }
    }
}

fn pair_cost_now_cents(
    side: OpportunitySide,
    trigger_entry_price_cents: Option<u8>,
    yes_ask_now: Option<u8>,
    no_ask_now: Option<u8>,
) -> Option<u16> {
    let entry = trigger_entry_price_cents? as u16;

    match side {
        OpportunitySide::BuyYes => {
            let no_ask = no_ask_now? as u16;
            Some(entry + no_ask)
        }
        OpportunitySide::BuyNo => {
            let yes_ask = yes_ask_now? as u16;
            Some(entry + yes_ask)
        }
    }
}

async fn maybe_record_leadlag(
    cfg: &Config,
    leadlag: &crate::leadlag::SharedLeadLag,
    ticker: &str,
    book: &Book,
) -> Result<()> {
    let now = now_ms();

    let yes_bid_now = book.best_yes_bid();
    let no_bid_now = book.best_no_bid();
    let yes_ask_now = book.best_yes_ask();
    let no_ask_now = book.best_no_ask();

    let mut g = leadlag.lock().await;
    let Some(pending) = g.pending.as_mut() else {
        return Ok(());
    };

    let ev = &mut pending.move_event;

    if ev.kalshi_ticker != ticker {
        // Market rotated. This pending event belongs to an old ticker and can never complete now.
        g.pending = None;
        g.recent_ticks.clear();
        return Ok(());
    }

    let age_ms = now.saturating_sub(ev.local_ts_ms);

    // Keep the event alive through the full horizon.
    // Only finalize and write it once the horizon expires.
    if age_ms > cfg.coinbase_leadlag_max_wait_ms {
        let ev_clone = ev.clone();
        g.pending = None;
        g.recent_ticks.clear();
        drop(g);

        crate::leadlag::append_leadlag_row(&cfg.coinbase_leadlag_file, &ev_clone).await?;
        return Ok(());
    }

    // Update rolling post-trigger bests
    match (ev.post.best_yes_bid, yes_bid_now) {
        (None, x) => ev.post.best_yes_bid = x,
        (Some(cur_best), Some(now_bid)) if now_bid > cur_best => {
            ev.post.best_yes_bid = Some(now_bid)
        }
        _ => {}
    }
    match (ev.post.best_no_bid, no_bid_now) {
        (None, x) => ev.post.best_no_bid = x,
        (Some(cur_best), Some(now_bid)) if now_bid > cur_best => {
            ev.post.best_no_bid = Some(now_bid)
        }
        _ => {}
    }
    match (ev.post.best_yes_ask, yes_ask_now) {
        (None, x) => ev.post.best_yes_ask = x,
        (Some(cur_best), Some(now_ask)) if now_ask > cur_best => {
            ev.post.best_yes_ask = Some(now_ask)
        }
        _ => {}
    }
    match (ev.post.best_no_ask, no_ask_now) {
        (None, x) => ev.post.best_no_ask = x,
        (Some(cur_best), Some(now_ask)) if now_ask > cur_best => {
            ev.post.best_no_ask = Some(now_ask)
        }
        _ => {}
    }

    let entry_edge = favorable_entry_edge_cents(
        ev.opportunity_side,
        ev.trigger_entry_price_cents,
        yes_ask_now,
        no_ask_now,
    );
    if entry_edge > ev.post.best_favorable_entry_edge_cents {
        ev.post.best_favorable_entry_edge_cents = entry_edge;
    }

    let exit_edge = favorable_exit_edge_cents(
        ev.opportunity_side,
        ev.trigger_entry_price_cents,
        yes_bid_now,
        no_bid_now,
    );
    if exit_edge > ev.post.best_favorable_exit_edge_cents {
        ev.post.best_favorable_exit_edge_cents = exit_edge;
    }

    // This is the direct metric for your pair-cost strategy.
    if let Some(pair_cost) = pair_cost_now_cents(
        ev.opportunity_side,
        ev.trigger_entry_price_cents,
        yes_ask_now,
        no_ask_now,
    ) {
        let replace = match ev.post.min_pair_cost_cents {
            None => true,
            Some(cur_min) => pair_cost < cur_min,
        };
        if replace {
            ev.post.min_pair_cost_cents = Some(pair_cost);
            ev.post.min_pair_cost_ts_ms = Some(now);
        }
    }

    // Record first response once, but do NOT close the event.
    if ev.first_response.is_none() {
        let min_move = cfg.coinbase_leadlag_min_kalshi_move_cents;

        let yes_up = match (ev.trigger_yes_bid, yes_bid_now) {
            (Some(b), Some(a)) => a >= b.saturating_add(min_move),
            _ => false,
        };
        let yes_down = match (ev.trigger_yes_bid, yes_bid_now) {
            (Some(b), Some(a)) => b >= a.saturating_add(min_move),
            _ => false,
        };
        let no_up = match (ev.trigger_no_bid, no_bid_now) {
            (Some(b), Some(a)) => a >= b.saturating_add(min_move),
            _ => false,
        };
        let no_down = match (ev.trigger_no_bid, no_bid_now) {
            (Some(b), Some(a)) => b >= a.saturating_add(min_move),
            _ => false,
        };

        let response_type = match ev.dir {
            crate::leadlag::MoveDir::Up => {
                if yes_up {
                    Some("yes_bid_up")
                } else if no_down {
                    Some("no_bid_down")
                } else {
                    None
                }
            }
            crate::leadlag::MoveDir::Down => {
                if no_up {
                    Some("no_bid_up")
                } else if yes_down {
                    Some("yes_bid_down")
                } else {
                    None
                }
            }
        };

        if let Some(response_type) = response_type {
            ev.first_response = Some(FirstResponse {
                response_type: response_type.to_string(),
                kalshi_response_ts_ms: now,
                lag_ms: age_ms,
                yes_bid_after: yes_bid_now,
                no_bid_after: no_bid_now,
                yes_ask_after: yes_ask_now,
                no_ask_after: no_ask_now,
            });
        }
    }

    Ok(())
}
