use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use crate::config::Config;
use crate::state::orders::{OrderRec, OrderStatus};
use crate::state::ticker::{Market, Mode};
use crate::types::{ExecCommand, RestingHint, Side, Tif, CC_PER_CENT};

const DOLLAR_CC: i64 = 100 * CC_PER_CENT; // 10000

fn desired_buy_qty(cfg: &Config, m: &Market, side: Side, t_rem: i64, window_s: i64) -> u64 {
    let yes = m.pos.yes_qty.max(0) as i64;
    let no  = m.pos.no_qty.max(0) as i64;

    let (my, other) = match side {
        Side::Yes => (yes, no),
        Side::No  => (no, yes),
    };

    // Only scale up if we're buying the side that's behind
    let gap = (other - my).max(0);
    if gap <= 0 {
        return 1;
    }

    // Taper: early more patient, late more urgent (0..1)
    let tf = taper_factor(t_rem, window_s);         // 1 early -> 0 late
    let urgency = (1.0 - tf).clamp(0.0, 1.0);       // 0 early -> 1 late

    // Base catch-up fraction; increases as we approach the end
    let mut frac = cfg.catchup_aggressiveness * (0.35 + 0.65 * urgency);

    // In Balance mode, allow more aggressive catch-up
    if m.mode == Mode::Balance {
        frac *= (1.0 + cfg.catchup_balance_boost);
    }

    // Convert gap -> desired qty chunk
    let q = ((gap as f64) * frac).ceil() as i64;

    // Always at least 1, never above cap, never more than full gap
    let q = q.clamp(1, gap).min(cfg.max_order_qty as i64);

    q as u64
}


fn best_maker_pc_for_side(cfg: &Config, m: &Market, side: Side, cap_cc: i64) -> Option<(u8, i64)> {
    let top = top_maker_price(cfg, m, side)?;
    let min_price = top.saturating_sub(cfg.maker_max_edge_cents);
    let p = best_price_under_pair_cap(m, side, top, min_price, cap_cc, true)?;
    let sim = m.pos.simulate_buy(side, p, 1);
    let pc = sim.pair_cost_cc()?;
    Some((p, pc))
}

fn unix_now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn has_pair(m: &Market) -> bool {
    m.pos.yes_qty > 0 && m.pos.no_qty > 0
}

fn qty_for(m: &Market, side: Side) -> i64 {
    match side {
        Side::Yes => m.pos.yes_qty,
        Side::No => m.pos.no_qty,
    }
}

fn avg_cc_for(m: &Market, side: Side) -> Option<i64> {
    match side {
        Side::Yes => m.pos.avg_yes_cc(),
        Side::No => m.pos.avg_no_cc(),
    }
}

fn max_missing_price_cents(cfg: &Config, m: &Market, existing_avg_cc: i64) -> u8 {
    // Use a looser cap late-window so you can finish hedged.
    let cap_cc = if m.mode == Mode::Balance {
        cfg.balance_pair_cc.max(DOLLAR_CC)
    } else {
        cfg.bootstrap_pair_cc
    };

    let max_cc = cap_cc - existing_avg_cc;
    if max_cc <= 0 {
        return 0;
    }
    (max_cc / CC_PER_CENT).clamp(0, cfg.max_buy_price_cents as i64) as u8
}

fn can_rescue_existing(cfg: &Config, m: &Market, existing: Side) -> Option<(u8, i64)> {
    let existing_avg_cc = avg_cc_for(m, existing)?;
    let existing_avg_cents = (existing_avg_cc / CC_PER_CENT).clamp(0, 100) as u8;

    let p = top_maker_price(cfg, m, existing)?;
    if p >= existing_avg_cents {
        return None; // would not reduce avg
    }

    let sim = m.pos.simulate_buy(existing, p, 1);
    let new_avg_cc = match existing {
        Side::Yes => sim.avg_yes_cc()?,
        Side::No => sim.avg_no_cc()?,
    };

    let improve_cc = existing_avg_cc - new_avg_cc;
    if improve_cc < cfg.bootstrap_rescue_min_improve_cc {
        return None;
    }

    Some((p, improve_cc))
}

fn choose_bootstrap_side_simple(cfg: &Config, m: &Market) -> Side {
    // Flat: just pick cheaper ask (hedge_side already does that)
    if m.pos.yes_qty == 0 && m.pos.no_qty == 0 {
        return hedge_side(m);
    }

    // One-sided: decide whether to work missing or do a rescue buy
    let existing = if m.pos.yes_qty > 0 { Side::Yes } else { Side::No };
    let missing = existing.other();

    if m.mode == Mode::Balance {
        return missing; // force hedge late window
    }

    let existing_avg_cc = avg_cc_for(m, existing).unwrap_or(0);
    let max_missing = max_missing_price_cents(cfg, m, existing_avg_cc);
    let ask_missing = m.book.implied_ask(missing);

    // If we can hedge under our cap at current market, do it.
    if ask_missing.map(|a| a <= max_missing).unwrap_or(false) {
        return missing;
    }

    // otherwise rescue-buy if it meaningfully improves avg and within one-sided limit
    if qty_for(m, existing) < cfg.bootstrap_max_one_side_qty {
        if can_rescue_existing(cfg, m, existing).is_some() {
            return existing;
        }
    }

    // Park bid on missing and wait for a swing
    missing
}

/// Fallback time remaining if we don’t have close_ts/open_ts.
fn time_remaining_s(now_s: i64, window_s: i64) -> i64 {
    let w = window_s.max(1);
    let start = (now_s / w) * w;
    let end = start + w;
    (end - now_s).max(0)
}

/// Compute the “true” window length from REST timestamps if available:
/// window_s = close_ts - open_ts, else cfg.window_s.
fn effective_window_s(cfg: &Config, m: &Market) -> i64 {
    match (m.open_ts, m.close_ts) {
        (Some(o), Some(c)) if c > o => (c - o).max(1),
        _ => cfg.window_s.max(1),
    }
}

/// Compute time remaining from REST close_ts if available:
/// t_rem = close_ts - now.
/// If close_ts missing but open_ts exists, approximate close = open + window_s.
/// Else fallback to epoch-bucket method.
fn effective_time_remaining_s(cfg: &Config, m: &Market, now_s: i64, window_s: i64) -> i64 {
    if let Some(c) = m.close_ts {
        return (c - now_s).max(0);
    }
    if let Some(o) = m.open_ts {
        return ((o + window_s) - now_s).max(0);
    }
    time_remaining_s(now_s, window_s)
}

/// “Taper” goes from 1.0 early to 0.0 at the end.
/// We use this to reduce risk-taking near settlement.
fn taper_factor(t_rem: i64, window_s: i64) -> f64 {
    let w = window_s.max(1) as f64;
    (t_rem as f64 / w).clamp(0.0, 1.0)
}

fn allowed_imbalance(cfg: &Config, t_rem: i64) -> f64 {
    if t_rem <= cfg.balance_s {
        cfg.late_imbalance_cap
    } else {
        cfg.early_imbalance_cap
    }
}

/// IMPORTANT: This must use the *actual* window length (window_s), not cfg.window_s.
/// accumulate_s means “first X seconds from open”
fn pick_mode(cfg: &Config, t_rem: i64, window_s: i64) -> Mode {
    if t_rem <= cfg.balance_s {
        // println!("Balance Mode: TRem: {:#?}", t_rem);
        Mode::Balance
    } else if t_rem > (window_s - cfg.accumulate_s) {
        // println!("Accumulate Mode: TRem: {:#?}", t_rem);
        Mode::Accumulate
    } else {
        // println!("Hedge Mode: TRem: {:#?}", t_rem);
        Mode::Hedge
    }
}

fn last_taker(m: &Market, side: Side) -> Option<Instant> {
    match side {
        Side::Yes => m.last_taker_yes,
        Side::No => m.last_taker_no,
    }
}

fn set_last_taker(m: &mut Market, side: Side, t: Instant) {
    match side {
        Side::Yes => m.last_taker_yes = Some(t),
        Side::No => m.last_taker_no = Some(t),
    }
}

/// Compute the “top maker price” for a side:
/// - starts near best bid
/// - bounded by post-only constraint (must be strictly below implied ask)
fn top_maker_price(cfg: &Config, m: &Market, side: Side) -> Option<u8> {
    let best = m.book.best_bid(side)?;
    let improve = if m.mode == Mode::Balance {
        cfg.maker_improve_tick_balance
    } else {
        cfg.maker_improve_tick
    };

    let mut p = best.saturating_add(improve).min(cfg.max_buy_price_cents);

    // Post-only: must not cross the implied ask.
    if let Some(ask) = m.book.implied_ask(side) {
        if ask == 0 { return None; }
        p = p.min(ask.saturating_sub(1));
    }
    Some(p)
}

fn best_price_under_pair_cap_qty(
    m: &Market,
    side: Side,
    max_price: u8,
    min_price: u8,
    cap_cc: i64,
    require_noworse: bool,
    qty: i64,
) -> Option<u8> {
    let old_pair = m.pos.pair_cost_cc();
    if min_price > max_price { return None; }

    for p in (min_price..=max_price).rev() {
        let sim = m.pos.simulate_buy(side, p, qty);
        let Some(new_pc) = sim.pair_cost_cc() else { continue; };

        if new_pc > cap_cc { continue; }

        if require_noworse {
            if let Some(old_pc) = old_pair {
                if new_pc > old_pc { continue; }
            }
        }

        return Some(p);
    }
    None
}


/// Search downward for a price that satisfies pair-cost constraints.
/// cap_cc is in cent-cents.
fn best_price_under_pair_cap(
    m: &Market,
    side: Side,
    max_price: u8,
    min_price: u8,
    cap_cc: i64,
    require_noworse: bool,
) -> Option<u8> {
    let old_pair = m.pos.pair_cost_cc();

    if min_price > max_price { return None; }

    for p in (min_price..=max_price).rev() {
        let sim = m.pos.simulate_buy(side, p, 1);
        let Some(new_pc) = sim.pair_cost_cc() else {
            // If we don’t have both sides yet, pair_cost is undefined.
            continue;
        };

        if new_pc > cap_cc { continue; }

        if require_noworse {
            if let Some(old_pc) = old_pair {
                if new_pc > old_pc { continue; }
            }
        }

        return Some(p);
    }

    None
}

/// If we have a resting hint and it’s too old, cancel it.
/// We do NOT cancel constantly; this is only for “stale” orders.
fn cancel_stale_if_needed(cfg: &Config, ticker: &str, m: &mut Market, now: Instant) -> Option<ExecCommand> {
    for side in [Side::Yes, Side::No] {
        let Some(h) = m.resting_hint(side).as_ref().cloned() else { continue; };
        let Some(order_id) = h.order_id.clone() else { continue; };

        let age_ms = now.duration_since(h.created_at).as_millis() as u64;
        if age_ms < cfg.min_resting_life_ms { continue; }

        // If we already requested cancel, don’t spam cancel every tick.
        if let Some(t0) = h.cancel_requested_at {
            let since = now.duration_since(t0).as_millis() as u64;
            if since < cfg.cancel_retry_ms { continue; }
        }

        if age_ms >= cfg.cancel_stale_ms {
            // Mark cancel requested in hint.
            if let Some(hm) = m.resting_hint_mut(side).as_mut() {
                hm.cancel_requested_at = Some(now);
            }
            return Some(ExecCommand::CancelOrder {
                ticker: ticker.to_string(),
                order_id,
            });
        }
    }
    None
}

/// Signal-free working side selection, but "pair-cost smart".
/// Chooses the side that would most reduce avg_yes+avg_no if a 1-lot maker fill happens.
///
/// Priority:
/// 1) If forced balance (late window / too imbalanced): hedge side
/// 2) If one-sided: buy missing side
/// 3) If both sides exist: pick the side with best simulated pair-cost improvement
fn choose_working_side_simple(cfg: &Config, m: &Market, t_rem: i64) -> Side {
    let imbalance_cap = allowed_imbalance(cfg, t_rem);

    // If we're forced to balance, always hedge.
    if m.mode == Mode::Balance || m.pos.imbalance_ratio() > imbalance_cap {
        return hedge_side(m);
    }

    // If we're one-sided, prioritize getting the missing side first.
    if m.pos.yes_qty > 0 && m.pos.no_qty == 0 {
        return Side::No;
    }
    if m.pos.no_qty > 0 && m.pos.yes_qty == 0 {
        return Side::Yes;
    }

    // Flat: prefer cheaper implied as (hedge_side already does this)
    if m.pos.yes_qty == 0 && m.pos.no_qty == 0 {
        return hedge_side(m);
    }

    // Both sides exist: choose side that yields best (lowest) simulated pair cost
    // from a 1-lot maker fill at our best quote.
    let old_pc = m.pos.pair_cost_cc().unwrap_or(i64::MAX);
    let cap_target = cfg.target_pair_cc;

    // If we're already under target, stay under target and don't worsen.
    // If we're above target, allow any improvement (new_pc <= old_pc).
    let cap_cc = if old_pc <= cap_target {
        cap_target
    } else {
        old_pc
    };

    let mut best: Option<(Side, i64, u8, u8)> = None;

    for side in [Side::Yes, Side::No] {
        // Respect imbalance cap (don't choose side that would push beyond allowed imbalance)
        let would = m.pos.simulate_buy(side, 0, 1);
        if would.imbalance_ratio() > imbalance_cap {
            continue;
        }

        // Candidate maker quote band
        let Some(top) = top_maker_price(cfg, m, side) else {
            continue;
        };
        let min_price = top.saturating_sub(cfg.maker_max_edge_cents);

        // Find a price in [min_price..top] that keeps us under cap_cc and (otionally doesn't worsen)
        let Some(p) = best_price_under_pair_cap(m, side, top, min_price, cap_cc, true) else {
            continue;
        };

        // Simulate a 1-lot fill at p and evaluate new pair cost
        let sim = m.pos.simulate_buy(side, p, 1);
        let Some(new_pc) = sim.pair_cost_cc() else {
            continue;
        };

        // Tie-break by tighter spread (more likely to fill /more efficient quoting)
        let bid = m.book.best_bid(side).unwrap_or(0);
        let ask = m.book.implied_ask(side).unwrap_or(100);
        let spread = ask.saturating_sub(bid);

        match best {
            None => best = Some((side, new_pc, p, spread)),
            Some((_, best_pc, _, best_spread)) => {
                if new_pc < best_pc || (new_pc == best_pc && spread < best_spread) {
                    best = Some((side, new_pc, p, spread));
                }
            }
        }
    }
    best.map(|(s, _, _, _)| s).unwrap_or_else(|| hedge_side(m))
}

fn hedge_side(m: &Market) -> Side {
    if m.pos.yes_qty < m.pos.no_qty {
        Side::Yes
    } else if m.pos.no_qty < m.pos.yes_qty {
        Side::No
    } else {
        // Flat/balanced: prefer cheaper ask (fallback to Yes if missing)
        match (m.book.implied_ask(Side::Yes), m.book.implied_ask(Side::No)) {
            (Some(ay), Some(an)) => if ay <= an { Side::Yes } else { Side::No },
            (Some(_), None) => Side::Yes,
            (None, Some(_)) => Side::No,
            _ => Side::Yes,
        }
    }
}

/// Opportunistic taker:
/// If the implied ask is “cheap enough” to improve (or keep under) our pair-cost,
/// place an IOC buy at the ask (limit at ask, post_only=false).
fn maybe_opportunistic_taker(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    window_s: i64,
    desired_side: Side,
) -> Option<ExecCommand> {
    // ---------------BOOTSTRAP TAKER-----------------
    // Until we have both sides, only taker in tight spreads
    // or when we're forcing balance late window.
    if !has_pair(m) {
        let side = desired_side;
        let Some(ask) = m.book.implied_ask(side) else {return None; };
        if ask > cfg.max_buy_price_cents {
            return None;
        }

        if let Some(last) = last_taker(m, side) {
            if (now.duration_since(last).as_millis() as u64) < cfg.taker_cooldown_ms {
                return None; 
            }
        }

        // One-sided: only taker the MISSING side and only if within cap
        if m.pos.yes_qty > 0 || m.pos.no_qty > 0 {
            let existing = if m.pos.yes_qty > 0 {
                Side::Yes
            } else {
                Side::No
            };
            let missing = existing.other();
            if side != missing {
                return None; // never taker rescue, maker only
            }

            let existing_avg_cc = avg_cc_for(m, existing)?;
            let max_missing = max_missing_price_cents(cfg, m, existing_avg_cc);
            if ask > max_missing {
                return None;
            }

            // If not Balance mode, require a tight spread to cross
            if m.mode != Mode::Balance {
                let best = m.book.best_bid(side)?;
                if ask > best.saturating_add(cfg.aggressive_tick) {
                    return None;
                }
            }
        } else {
            // Flat: only take if tight spread (otherwise maker quote)
            if m.mode != Mode::Balance {
                let best = m.book.best_bid(side)?;
                if ask > best.saturating_add(cfg.aggressive_tick) {
                    return None;
                }
            }
        }

        let qty = desired_buy_qty(cfg, m, side, t_rem, window_s);
        let client_order_id = uuid::Uuid::new_v4();
        m.orders.insert_pending(OrderRec {
            ticker: ticker.to_string(),
            side,
            price_cents: ask,
            qty: qty,
            tif: Tif::Ioc,
            post_only: false,
            order_id: None,
            client_order_id,
            status: OrderStatus::PendingAck,
            created_at: now,
            filled_qty: 0,
        });

        set_last_taker(m, side, now);

        return Some(ExecCommand::PlaceOrder {
            ticker: ticker.to_string(),
            side,
            price_cents: ask,
            qty: qty,
            tif: Tif::Ioc,
            post_only: false,
            client_order_id,
        });
    }

    let imbalance_cap = allowed_imbalance(cfg, t_rem);
    let must_balance = m.mode == Mode::Balance || m.pos.imbalance_ratio() > imbalance_cap;
    let hedge = hedge_side(m);
    let desperate = must_balance && t_rem <= cfg.taker_desperate_s;

    let cap_target = cfg.target_pair_cc;
    let cap_balance = cfg.balance_pair_cc.clamp(0,DOLLAR_CC);
    let old_pc = m.pos.pair_cost_cc().unwrap_or(i64::MAX);

    // Same "improvement-mode" cap as maker quoting to somparison is fair
    let cap_for_maker = if old_pc <= cap_target {
        cap_target
    } else {
        old_pc
    };

    let mut best: Option<(Side, u8, i64, u64)> = None;

    for side in [Side::Yes, Side::No] {
        // If we already have a maker resting on this side, give it time before paying taker fees.
        if !desperate {
            if let Some(h) = m.resting_hint(side).as_ref() {
                let age_ms = now.duration_since(h.created_at).as_millis() as u64;
                if age_ms < cfg.maker_first_ms {
                    continue;
                }
            }
        }

        if must_balance && side != hedge {
            continue; // in balance mode only buy the hedge side
        }

        let Some(ask) = m.book.implied_ask(side) else { continue; };
        if ask > cfg.max_buy_price_cents { continue; }

        if let Some(last) = last_taker(m, side) {
            if (now.duration_since(last).as_millis() as u64) < cfg.taker_cooldown_ms {
                continue;
            }
        }

        let qty = desired_buy_qty(cfg, m, side, t_rem, window_s);

        if !must_balance {
            let would = m.pos.simulate_buy(side, 0, qty as i64); // price doesn't matter for imbalance_ratio
            if would.imbalance_ratio() > imbalance_cap {
                continue;
            }
        }

        let sim = m.pos.simulate_buy(side, ask, qty as i64);
        let Some(new_pc) = sim.pair_cost_cc() else {
            continue;
        };

        let cap_when_balancing = if old_pc <= cap_target {
            cap_target // once under target, never exceed target
        } else {
            cap_balance // above target, allow balance cap behavior
        };

        // If we MUST balance, allow up to balance cap even if it doesn't "improve"
        if must_balance {
            // Balance mode: prefer maker until the final 'desperate' window.
            if !desperate {
                continue;
            }

            if new_pc <= cap_when_balancing {
                // keep your "pick best" logic
                match best {
                    None => best = Some((side, ask, new_pc, qty)),
                    Some((_, _, bp, _)) if new_pc < bp => best = Some((side, ask, new_pc, qty)),
                    _ => {}
                }
            }
            continue;
        }

        // --------- taker vs maker gate ----------
        let maker_pc = best_maker_pc_for_side(cfg, m, side, cap_for_maker)
            .map(|(_p, pc)| pc)
            .unwrap_or(i64::MAX);

        let bid = m.book.best_bid(side).unwrap_or(0);
        let tight = ask <= bid.saturating_add(cfg.aggressive_tick);

        let improve = old_pc - new_pc; // positive = better
        if improve <= 0 {
            continue; // never pay taker fee to be worse or equal
        }

        // Tight-spread crossings: only when it's a BIG improvement (worth fee)
        if tight {
            if improve < cfg.taker_big_improve_cc {
                continue;
            }
        } else {
            // Wide spread: require meaningful improvement AND taker must be at least as good as maker
            if improve < cfg.min_taker_improve_cc {
                continue;
            }
            if new_pc > maker_pc {
                continue; // maker is better, so don't pay taker fee
            }
        }

        // If we get here, taker is allowed
        match best {
            None => best = Some((side, ask, new_pc, qty)),
            Some((_, _, bp, _)) if new_pc < bp => best = Some((side, ask, new_pc, qty)),
            _ => {}
        }
    }

    let (side, ask, _new_pc, qty) = best?;

    let client_order_id = uuid::Uuid::new_v4();
    m.orders.insert_pending(OrderRec {
        ticker: ticker.to_string(),
        side,
        price_cents: ask,
        qty: qty,
        tif: Tif::Ioc,
        post_only: false,
        order_id: None,
        client_order_id,
        status: OrderStatus::PendingAck,
        created_at: now,
        filled_qty: 0,
    });

    set_last_taker(m, side, now);

    Some(ExecCommand::PlaceOrder {
        ticker: ticker.to_string(),
        side,
        price_cents: ask,
        qty: qty,
        tif: Tif::Ioc,
        post_only: false,
        client_order_id,
    })
}

/// Maker quote logic:
/// Maintain at most one resting order, don’t churn, and keep prices within pair-cost constraints.
fn maybe_maker_quote(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    window_s: i64,
    desired_side: Side,
) -> Option<ExecCommand> {
    // let desired_side = choose_working_side(cfg, m, t_rem, score);

    let other = desired_side.other();
    if let Some(h) = m.resting_hint(other).as_ref().cloned() {
        if let Some(order_id) = h.order_id.clone() {
            let age_ms = now.duration_since(h.created_at).as_millis() as u64;
            if age_ms >= cfg.min_resting_life_ms {
                if h.cancel_requested_at.is_none()
                    || now.duration_since(h.cancel_requested_at.unwrap()).as_millis() as u64 >= cfg.cancel_retry_ms
                {
                    if let Some(hm) = m.resting_hint_mut(other).as_mut() {
                        hm.cancel_requested_at = Some(now);
                    }
                    return Some(ExecCommand::CancelOrder {
                        ticker: ticker.to_string(),
                        order_id,
                    });
                }
            }
        }
    }

    let cap_target = cfg.target_pair_cc;
    let cap_safe = cfg.safe_pair_cc;
    let cap_balance = cfg.balance_pair_cc.clamp(0,DOLLAR_CC);

    let top = top_maker_price(cfg, m, desired_side)?;
    let min_price = top.saturating_sub(cfg.maker_max_edge_cents);

    // ----------BOOTSTRAP MAKER QUOTE------------
    if !has_pair(m) {
        // Flat: just quote near top maker price on desired_side.
        if m.pos.yes_qty == 0 && m.pos.no_qty == 0 {
            return place_or_manage_resting(cfg, ticker, m, now, desired_side, top);
        }

        // one-sided bootstrap:
        let existing = if m.pos.yes_qty > 0 {
            Side::Yes
        } else {
            Side::No
        };

        let missing = existing.other();

        if desired_side == missing {
            let existing_avg_cc = avg_cc_for(m, existing)?;
            let max_missing = max_missing_price_cents(cfg, m, existing_avg_cc);
            if max_missing == 0 {
                return None;
            }
            // Allow "deep" quotes in bootstrap (do NOT enforce maker_max_edge here)
            let p = top.min(max_missing);
            return place_or_manage_resting(cfg, ticker, m,  now, desired_side, p);
        } else {
            // Rescue-buy side: only if it improves avg and we haven't exceeded max one sided qty
            if qty_for(m, existing) >= cfg.bootstrap_max_one_side_qty {
                return None;
            }
            let (p, _improve) = can_rescue_existing(cfg, m, existing)?;
            return place_or_manage_resting(cfg, ticker, m, now, desired_side, p);
        }
    }

    // --------------NORMAL (PAIRED) MAKER QUOTE--------
    // 1) Decide desired qty first (so price selection is qty-aware)
    let mut qty: u64 = desired_buy_qty(cfg, m, desired_side, t_rem, window_s);
    if qty == 0 { qty = 1; } // defensive

    let old_pc = m.pos.pair_cost_cc().unwrap_or(cap_safe);

    // Balance-mode ratchet:
    // - if we're already under target, NEVER allow maker fills to push us above target
    // - otherwise, use the looser balance cap
    let cap_when_balancing = if old_pc <= cap_target {
        cap_target
    } else {
        cap_balance
    };

    // 2) Choose which cap rules apply
    // let (cap_cc, require_noworse) = if m.mode == Mode::Balance {
    //     (cap_when_balancing, false)
    // } else if old_pc <= cap_target {
    //     (cap_target, true)
    // } else {
    //     (old_pc, true)
    // };
    let unbalanced = !m.pos.is_balanced();
    let (cap_cc, require_noworse) = if m.mode == Mode::Balance {
        (cap_when_balancing, false)
    } else if unbalanced {
        // allow hedging even if it worsens pair-cost, but keep it <= target
        (cap_target, false)
    } else if old_pc <= cap_target {
        (cap_target, true)
    } else {
        (old_pc, true)
    };

        // 3) Find a price using qty-aware simulation.
    // If qty is too large to fit caps, fall back by shrinking qty.
    let mut p_opt = best_price_under_pair_cap_qty(
        m,
        desired_side,
        top,
        min_price,
        cap_cc,
        require_noworse,
        qty as i64,
    );

    while p_opt.is_none() && qty > 1 {
    qty = (qty / 2).max(1);
    p_opt = best_price_under_pair_cap_qty(
        m,
        desired_side,
        top,
        min_price,
        cap_cc,
        require_noworse,
        qty as i64,
    );
    }

    let p = p_opt?;
    // 4) If we already have a resting order on this side, manage it.
    if let Some(existing) = m.resting_hint(desired_side).as_ref().cloned() {
        // Look up existing order qty from Orders (no need to store qty in RestingHint)
        let existing_remaining = m.orders.by_client
            .get(&existing.client_order_id)
            .map(|r| r.qty.saturating_sub(r.filled_qty))
            .unwrap_or(1);

        // Only force a resize if we want MORE (avoids churn when gap shrinks)
        let want_upsize = qty > existing_remaining;

        // If price is identical and we don't need to upsize, leave it alone.
        if existing.price_cents == p && !want_upsize {
            return None;
        }

        let age_ms = now.duration_since(existing.created_at).as_millis() as u64;
        if age_ms < cfg.min_resting_life_ms {
            return None;
        }

        if let Some(t0) = existing.cancel_requested_at {
            let since = now.duration_since(t0).as_millis() as u64;
            if since < cfg.cancel_retry_ms {
                return None;
            }
        }

        let drift = existing.price_cents.abs_diff(p);
        let should_cancel = drift >= cfg.cancel_drift_cents || want_upsize;

        if should_cancel {
            let Some(order_id) = existing.order_id.clone() else { return None; };

            if let Some(hm) = m.resting_hint_mut(desired_side).as_mut() {
                hm.cancel_requested_at = Some(now);
            }

            return Some(ExecCommand::CancelOrder {
                ticker: ticker.to_string(),
                order_id,
            });
        }

        return None;
    }

    // 5) Place new resting order with qty
    let client_order_id = uuid::Uuid::new_v4();

    m.orders.insert_pending(OrderRec {
        ticker: ticker.to_string(),
        side: desired_side,
        price_cents: p,
        qty,
        tif: Tif::Gtc,
        post_only: true,
        order_id: None,
        client_order_id,
        status: OrderStatus::PendingAck,
        created_at: now,
        filled_qty: 0,
    });

    let queue_ahead = match desired_side {
        Side::Yes => m.book.yes_bids[p as usize],
        Side::No  => m.book.no_bids[p as usize],
    };

    *m.resting_hint_mut(desired_side) = Some(RestingHint {
        side: desired_side,
        price_cents: p,
        created_at: now,
        cancel_requested_at: None,
        client_order_id,
        order_id: None,
        // Paper trading
        queue_ahead,
    });

    Some(ExecCommand::PlaceOrder {
        ticker: ticker.to_string(),
        side: desired_side,
        price_cents: p,
        qty,
        tif: Tif::Gtc,
        post_only: true,
        client_order_id,
    })
}

// Small helper to reuse your existing "one resting order per side"
fn place_or_manage_resting(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    side: Side,
    p: u8,
) -> Option<ExecCommand> {
    if let Some(existing) = m.resting_hint(side).as_ref().cloned() {
        if existing.price_cents == p {
            return None;
        }

        let age_ms = now.duration_since(existing.created_at).as_millis() as u64;
        if age_ms < cfg.min_resting_life_ms {
            return None;
        }

        if let Some(t0) = existing.cancel_requested_at {
            let since = now.duration_since(t0).as_millis() as u64;
            if since < cfg.cancel_retry_ms {
                return None;
            }
        }

        let drift = existing.price_cents.abs_diff(p);
        if drift >= cfg.cancel_drift_cents {
            let Some(order_id) = existing.order_id.clone() else {
                return None;
            };
            if let Some(hm) = m.resting_hint_mut(side).as_mut() {
                hm.cancel_requested_at = Some(now);
            }
            return Some(ExecCommand::CancelOrder {
                ticker: ticker.to_string(),
                order_id,
            });
        }
    return None;
    }

    let client_order_id = uuid::Uuid::new_v4();
    m.orders.insert_pending(OrderRec {
        ticker: ticker.to_string(),
        side,
        price_cents: p,
        qty: 1,
        tif: Tif::Gtc,
        post_only: true,
        order_id: None,
        client_order_id,
        status: OrderStatus::PendingAck,
        created_at: now,
        filled_qty: 0,
    });

    let queue_ahead = match side {
    Side::Yes => m.book.yes_bids[p as usize],
    Side::No => m.book.no_bids[p as usize],
    };
    *m.resting_hint_mut(side) = Some(RestingHint {
        side,
        price_cents: p,
        created_at: now,
        cancel_requested_at: None,
        client_order_id,
        order_id: None,
        // PAPER TRADING
        queue_ahead,
    });

    Some(ExecCommand::PlaceOrder {
        ticker: ticker.to_string(),
        side,
        price_cents: p,
        qty: 1,
        tif: Tif::Gtc,
        post_only: true,
        client_order_id,
    })
}

pub fn decide(cfg: &Config, ticker: &str, m: &mut Market) -> Option<ExecCommand> {
    let now_s = unix_now_s();
    let now = Instant::now();

    // If market_manager already told us close_ts, stop trading after that.
    if let Some(close_ts) = m.close_ts {
        if now_s >= close_ts {
            return None;
        }
    }

    // Use REST-derived window size and time remaining.
    let window_s = effective_window_s(cfg, m);
    let t_rem = effective_time_remaining_s(cfg, m, now_s, window_s);

    // Mode uses actual window_s (not cfg.window_s).
    m.mode = pick_mode(cfg, t_rem, window_s);
    // println!("Current Mode: {:#?}", m.mode);

    let desired_side = if has_pair(m) {
        choose_working_side_simple(cfg, m, t_rem)
    } else {
        choose_bootstrap_side_simple(cfg, m)
    };

    // 0) Cancel stale resting orders (but never churn fast).
    if let Some(cmd) = cancel_stale_if_needed(cfg, ticker, m, now) {
        return Some(cmd);
    }

    // 1) Opportunistic taker (cost-driven): if ask is cheap enough to improve/keep caps.
    if let Some(cmd) = maybe_opportunistic_taker(cfg, ticker, m, now, t_rem, window_s, desired_side) {
        return Some(cmd);
    }

    // 2) Maker quoting (resting) with churn control.
    maybe_maker_quote(cfg, ticker, m, now, t_rem, window_s, desired_side)
}
