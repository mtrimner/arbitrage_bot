use std::time::Instant;
use tracing::info;

use crate::state::{Shared};
use crate::state::orders::OrderStatus;
use crate::types::{Side, Tif};
use crate::state::ticker::Market;

pub fn paper_on_delta_queue(m: &mut Market, side: Side, price: u8, delta: i64) {
    if delta >= 0 { return; }

    if let Some(h) = m.resting_hint_mut(side).as_mut() {
        if h.price_cents == price && h.order_id.is_some() {
            // Treat negative delta as “some liquidity at this level disappeared”
            // and allow it to reduce queue ahead (imperfect but useful).
            h.queue_ahead = (h.queue_ahead + delta).max(0);
        }
    }
}

pub fn paper_on_trade_fill(ticker: &str, m: &mut Market, taker_side: Side, yes_price: u8, no_price: u8, count: i64) {
    let fillable = count.max(0) as u64;
    if fillable == 0 { return; }

    // maker side is the opposite side of taker
    let maker_side = taker_side.other();
    let maker_price = match maker_side {
        Side::Yes => yes_price,
        Side::No  => no_price,
    };

    let (client_id, posted_price, remaining_after_queue_u64) = {
        let Some(h) = m.resting_hint_mut(maker_side).as_mut() else { return; };
        if h.order_id.is_none() { return; }            // not acked yet

                // --------- IMPORTANT CHANGE ----------
        // If the market traded at/through our posted maker price, we should be fill-eligible.
        //
        // For a resting BUY at h.price_cents:
        // - maker_price > h.price_cents  => trade happened above our bid; cannot have hit us
        // - maker_price == h.price_cents => traded exactly at our level
        // - maker_price < h.price_cents  => traded through our level (gap/skip); assume we were crossed
        if maker_price > h.price_cents {
            return;
        }

        // If the tape traded below our price, assume our level was swept through;
        // don't let stale "queue_ahead at our exact price" prevent fills.
        if maker_price < h.price_cents {
            h.queue_ahead = 0;
        }
        // ------------------------------------

        // Consume queue ahead first
        let mut remaining = fillable as i64;

        if h.queue_ahead > 0 {
            let consume = h.queue_ahead.min(remaining);
            h.queue_ahead -= consume;
            remaining -= consume;
        }

        if remaining <= 0 { return; }

        (h.client_order_id, h.price_cents, remaining as u64)
    };
        let order_remaining = match m.orders.by_client.get(&client_id) {
        Some(rec) => rec.qty.saturating_sub(rec.filled_qty),
        None => return,
    };
    
    if order_remaining == 0 { return; }

    let fill_qty = order_remaining.min(remaining_after_queue_u64);
    if fill_qty == 0 { return; }

    // Option A: fill at OUR posted maker price (conservative)
    let fill_price = posted_price;

    info!(?maker_side, maker_price, fill_price, fill_qty, "PAPER maker filled");
    m.pos.apply_fill(maker_side, fill_price, fill_qty as i64);
    crate::report::log_position(ticker, &m.pos);

    let fully = m.orders.on_fill_by_client(client_id, fill_qty);

    if matches!(fully, Some(true)) {
        // clear resting hint when fully filled
        *m.resting_hint_mut(maker_side) = None;
    }
}


pub async fn paper_place(
    shared: &Shared,
    ticker: &str,
    side: Side,
    price_cents: u8,
    qty: u64,
    tif: Tif,
    post_only: bool,
    client_order_id: uuid::Uuid,
    reject_postonly_cross: bool,
) {
    let Some(ts) = shared.tickers.get(ticker) else { return; };
    let mut g = ts.mkt.write().await;

    // synthetic exchange order id
    let order_id = format!("paper-{}", uuid::Uuid::new_v4());
    g.orders.link_order_id(client_order_id, &order_id);

    // Post-only reject if it would cross *right now*
    if post_only && reject_postonly_cross && g.book.crosses_ask(side, price_cents) {
        info!(ticker, ?side, price_cents, qty, "PAPER reject post_only would-cross");
        g.orders.set_status_by_client(client_order_id, OrderStatus::Rejected);

        // Clear hint if we set one for this client_order_id
        if let Some(h) = g.resting_hint(side).as_ref() {
            if h.client_order_id == client_order_id {
                *g.resting_hint_mut(side) = None;
            }
        }

        ts.mark_dirty();
        shared.notify.notify_one();
        return;
    }

    match tif {
        Tif::Ioc => {
            // IOC: fill if limit >= implied ask
            let Some(ask) = g.book.implied_ask(side) else {
                info!(ticker, ?side, price_cents, "PAPER ioc reject no-ask");
                g.orders.set_status_by_client(client_order_id, OrderStatus::Rejected);
                ts.mark_dirty();
                shared.notify.notify_one();
                return;
            };

            if ask <= price_cents {
                let fill_qty = qty;
                info!(ticker, ?side, limit=price_cents, fill_price=ask, fill_qty, "PAPER ioc filled");
                g.pos.apply_fill(side, ask, fill_qty as i64);
                let _ = g.orders.on_fill_by_client(client_order_id, fill_qty);
                crate::report::log_position(ticker, &g.pos);
            } else {
                info!(ticker, ?side, limit=price_cents, ask, "PAPER ioc not-filled reject");
                g.orders.set_status_by_client(client_order_id, OrderStatus::Rejected);
            }

            ts.mark_dirty();
            shared.notify.notify_one();
        }

        Tif::Gtc => {
            // GTC: accept as resting
            info!(ticker, ?side, price_cents, qty, post_only, order_id=%order_id, "PAPER resting ack");

            g.orders.set_status_by_client(client_order_id, OrderStatus::Resting);

            // Fill in hint order_id so cancels work
            if let Some(h) = g.resting_hint_mut(side).as_mut() {
                if h.client_order_id == client_order_id {
                    h.order_id = Some(order_id);
                }
            }

            ts.mark_dirty();
            shared.notify.notify_one();
        }
    }
}

pub async fn paper_cancel(shared: &Shared, ticker: &str, order_id: &str) {
    let Some(ts) = shared.tickers.get(ticker) else { return; };
    let mut g = ts.mkt.write().await;

    // If already filled, do nothing (realistic race behavior)
    // (status lookup optional; we’ll just attempt cancel)
    g.orders.set_status_by_order(order_id, OrderStatus::Canceled);

    for side in [Side::Yes, Side::No] {
        if let Some(h) = g.resting_hint(side).as_ref() {
            if h.order_id.as_deref() == Some(order_id) {
                *g.resting_hint_mut(side) = None;
            }
        }
    }

    info!(ticker, order_id, "PAPER cancel ack");
    ts.mark_dirty();
    shared.notify.notify_one();
}
