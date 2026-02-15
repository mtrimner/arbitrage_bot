use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use crate::config::Config;
use crate::engine::signal;
use crate::state::orders::{OrderRec, OrderStatus};
use crate::state::ticker::{Market, Mode};
use crate::types::{ExecCommand, RestingHint, Side, Tif, CC_PER_CENT, SAFE_PAIR_CC, TARGET_PAIR_CC};

const DOLLAR_CC: i64 = 100 * CC_PER_CENT; // 10000
// ------------------- DEBUG STUFF -----------------
fn clamp01(x: f64) -> f64 { x.clamp(0.0, 1.0) }

fn sign01(x: f64) -> f64 {
    if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { 0.0 }
}

fn need_hedge(cfg: &Config, m: &Market, t_rem: i64) -> bool {
    let cap = allowed_imbalance(cfg, t_rem);
    !m.pos.is_balanced() && (m.mode == Mode::Balance || m.pos.imbalance_ratio() > cap)
}

// Don’t add to a side if the OTHER side is still zero.
// Example: YES>0 and NO==0 => block more YES; only buy NO until you have at least 1.
fn one_sided_block(m: &Market, side: Side) -> bool {
    match side {
        Side::Yes => m.pos.yes_qty > 0 && m.pos.no_qty == 0,
        Side::No  => m.pos.no_qty  > 0 && m.pos.yes_qty == 0,
    }
}

/// Top N (price, qty) bid levels for logging.
fn top_levels(arr: &[i64; 101], n: usize) -> Vec<(u8, i64)> {
    let mut out = Vec::with_capacity(n);
    for p in (0..=100).rev() {
        let q = arr[p];
        if q > 0 {
            out.push((p as u8, q));
            if out.len() >= n { break; }
        }
    }
    out
}

fn best_maker_pc_for_side(cfg: &Config, m: &Market, side: Side, cap_cc: i64) -> Option<(u8, i64)> {
    let top = top_maker_price(cfg, m, side)?;
    let min_price = top.saturating_sub(cfg.maker_max_edge_cents);
    let p = best_price_under_pair_cap(m, side, top, min_price, cap_cc, true)?;
    let sim = m.pos.simulate_buy(side, p, 1);
    let pc = sim.pair_cost_cc()?;
    Some((p, pc))
}

/// Matches the private helper in signal.rs (for logging depth_norm numbers).
fn weighted_top_n_qty(arr: &[i64; 101], n: usize) -> f64 {
    let mut acc = 0.0;
    let mut found = 0usize;

    for p in (0..=100).rev() {
        let q = arr[p];
        if q <= 0 { continue; }
        let w = (1.0 - 0.2 * (found as f64)).max(0.2);
        acc += (q as f64) * w;
        found += 1;
        if found >= n { break; }
    }

    acc
}

// ----------------------------------------------------------

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

fn choose_bootstrap_side(cfg: &Config, m: &Market, score: f64, conf: f64) -> Side {
    // Flat: optionally follow flow, otherwise pick cheaper ask (hedge_side does that)
    if m.pos.yes_qty == 0 && m.pos.no_qty == 0 {
        let hedge = hedge_side(m);
        let flow = momentum_side(score);
        let strength = score.abs() * conf.clamp(0.0, 1.0);
        if conf >= cfg.min_conf_for_flow && strength >= cfg.momentum_score_threshold {
            flow
        } else {
            hedge
        }
    } else {
        // One-sided: decide whether to work missing or do a rescue buy
        let existing = if m.pos.yes_qty > 0 {
            Side::Yes
        } else {
            Side::No
        };
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

        // otherwise, only rescue-buy if:
        // - we haven't exceeded max one-sided qty
        // - and the top maker price meaningfully improves our avg
        if qty_for(m, existing) < cfg.bootstrap_max_one_side_qty {
            if can_rescue_existing(cfg, m, existing).is_some() {
                return existing;
            }
        }
            // Park a bid on missing at/under cap and wait for a swing.
            missing
    }
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



/// Fallback “window id” when we don’t have open_ts.
/// (For BTC15M this usually still matches because starts align to 15m boundaries in UTC.)
fn window_id(now_s: i64, window_s: i64) -> i64 {
    now_s / window_s.max(1)
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

fn momentum_side(score: f64) -> Side {
    if score >= 0.0 { Side::Yes } else { Side::No }
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
    let mut p = best.saturating_add(cfg.maker_improve_tick).min(cfg.max_buy_price_cents);

    // Post-only: must not cross the implied ask.
    if let Some(ask) = m.book.implied_ask(side) {
        if ask == 0 { return None; }
        p = p.min(ask.saturating_sub(1));
    }
    Some(p)
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

fn cancel_wrong_side_if_needed(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
) -> Option<ExecCommand> {
    if !need_hedge(cfg, m, t_rem) {
        return None;
    }

    let wrong = hedge_side(m).other();
    let Some(h) = m.resting_hint(wrong).as_ref().cloned() else { return None; };
    let Some(order_id) = h.order_id.clone() else { return None; };

    let age_ms = now.duration_since(h.created_at).as_millis() as u64;
    if age_ms < cfg.min_resting_life_ms {
        return None;
    }

    if let Some(t0) = h.cancel_requested_at {
        let since = now.duration_since(t0).as_millis() as u64;
        if since < cfg.cancel_retry_ms {
            return None;
        }
    }

    if let Some(hm) = m.resting_hint_mut(wrong).as_mut() {
        hm.cancel_requested_at = Some(now);
    }

    Some(ExecCommand::CancelOrder {
        ticker: ticker.to_string(),
        order_id,
    })
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
    let cap_target = cfg.target_pair_cc.max(TARGET_PAIR_CC);

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

fn choose_working_side(cfg: &Config, m: &Market, t_rem: i64, score: f64, conf: f64) -> Side {
    let imbalance_cap = allowed_imbalance(cfg, t_rem);
    let imbr = m.pos.imbalance_ratio();

    let hedge = hedge_side(m);
    let flow  = if score >= 0.0 { Side::Yes } else { Side::No };

    let must_balance = m.mode == Mode::Balance || imbr > imbalance_cap;

    let conf01 = conf.clamp(0.0, 1.0);
    let strength = score.abs() * conf01;
    // println!("Choose Side Conf, Strngth {:#?}, {:#?}", conf, strength);
    let enter = cfg.momentum_score_threshold;
    // Keep hysteresis sane even if side_exit_mult is misconfigured.
    let exit  = (enter * cfg.side_exit_mult).min(enter);

    // Gate is about confidence/trust (only for entering / flipping into flow).
    let gate_ok = conf >= cfg.min_conf_for_flow;

    let prev = m.last_desired_side.unwrap_or(hedge);

    // "Flow state" = we were last choosing the current flow side.
    // (This matches what your logs show: in_flow_state=false when prev!=flow.)
    let in_flow_state = prev == flow;

    // Raw threshold hits (informational)
    let enter_hit   = strength >= enter;
    let exit_hit    = strength <= exit;
    let in_deadband = strength > exit && strength < enter;

    // Effective state transitions (actionable)
    let enter_trigger = !in_flow_state && enter_hit && gate_ok;
    let exit_trigger  =  in_flow_state && exit_hit;

    let (chosen, reason) = if must_balance {
        (hedge, "must_balance")
    } else if enter_trigger {
        (flow, "enter_flow")
    } else if exit_trigger {
        (hedge, "exit_to_hedge")
    } else if in_flow_state {
        // Once in flow, stay until exit threshold trips (conf gate does NOT kick you out).
        if in_deadband {
            (prev, "deadband_keep_flow")
        } else {
            (prev, "stay_flow")
        }
    } else {
        // Not in flow: stay on hedge side. (exit threshold is intentionally ignored here)
        if enter_hit && !gate_ok {
            (hedge, "enter_blocked_low_conf_stay_hedge")
        } else if in_deadband {
            (hedge, "deadband_stay_hedge")
        } else {
            (hedge, "stay_hedge")
        }
    };

    chosen
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

        let client_order_id = uuid::Uuid::new_v4();
        m.orders.insert_pending(OrderRec {
            ticker: ticker.to_string(),
            side,
            price_cents: ask,
            qty: 1,
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
            qty: 1,
            tif: Tif::Ioc,
            post_only: false,
            client_order_id,
        });
    }

    let imbalance_cap = allowed_imbalance(cfg, t_rem);
    let must_balance = m.mode == Mode::Balance || m.pos.imbalance_ratio() > imbalance_cap;
    let hedge = hedge_side(m);

    let cap_target = cfg.target_pair_cc.max(TARGET_PAIR_CC);
    let cap_balance = cfg.balance_pair_cc.max(DOLLAR_CC);
    let old_pc = m.pos.pair_cost_cc().unwrap_or(i64::MAX);

    // Same "improvement-mode" cap as maker quoting to somparison is fair
    let cap_for_maker = if old_pc <= cap_target {
        cap_target
    } else {
        old_pc
    };

    let mut best: Option<(Side, u8, i64)> = None;

    for side in [Side::Yes, Side::No] {
        if must_balance && side != hedge {
            continue; // in balance mode only buy the hedge side
        }

        if !must_balance {
            let would = m.pos.simulate_buy(side, 0, 1); // price doesn't matter for imbalance_ratio
            if would.imbalance_ratio() > imbalance_cap {
                continue;
            }
        }

        let Some(ask) = m.book.implied_ask(side) else { continue; };
        if ask > cfg.max_buy_price_cents { continue; }

        if let Some(last) = last_taker(m, side) {
            if (now.duration_since(last).as_millis() as u64) < cfg.taker_cooldown_ms {
                continue;
            }
        }

        let sim = m.pos.simulate_buy(side, ask, 1);
        let Some(new_pc) = sim.pair_cost_cc() else {
            continue;
        };

        // If we MUST balance, allow up to balance cap even if it doesn't "improve"
        if must_balance && new_pc <= cap_balance {
            match best {
                None => best = Some((side, ask, new_pc)),
                Some((_, _, bp)) if new_pc < bp => best = Some((side, ask, new_pc)),
                _ => {}
            }
            continue;
        }

        // --------- NEW: taker vs maker gate ----------
        let maker_pc = best_maker_pc_for_side(cfg, m, side, cap_for_maker)
            .map(|(_p, pc)| pc)
            .unwrap_or(i64::MAX);

        let bid = m.book.best_bid(side).unwrap_or(0);
        let tight = ask <= bid.saturating_add(cfg.aggressive_tick);

        // If spread isn't tight, only cross if taker is at least as good as maker on pair-cost.
        let taker_beats_maker = new_pc <= maker_pc;

        // Target hit is great, but still don't cross wide if maker is better.
        if new_pc <= cap_target && (tight || taker_beats_maker) {
            match best {
                None => best = Some((side, ask, new_pc)),
                Some((_, _, bp)) if new_pc < bp => best = Some((side, ask, new_pc)),
                _ => {}
            }
            continue;
        }

        let improve = old_pc - new_pc;
        if improve >= cfg.min_taker_improve_cc && (tight || taker_beats_maker) {
            match best {
                None => best = Some((side, ask, new_pc)),
                Some((_, _, bp)) if new_pc < bp => best = Some((side, ask, new_pc)),
                _ => {}
            }
        }
    }

    let (side, ask, _new_pc) = best?;

    let client_order_id = uuid::Uuid::new_v4();
    m.orders.insert_pending(OrderRec {
        ticker: ticker.to_string(),
        side,
        price_cents: ask,
        qty: 1,
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
        qty: 1,
        tif: Tif::Ioc,
        post_only: false,
        client_order_id,
    })
}

/// Momentum taker:
/// If score is strong and we’re allowed extra “flow-follow” buys, take at ask,
/// but do not violate safe_pair_cc.
#[allow(dead_code)]
fn maybe_momentum_taker(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    window_s: i64,
    raw_score: f64,
    score: f64,
    conf: f64,
) -> Option<ExecCommand> {
    if conf < cfg.min_conf_for_momentum {
        return None;
    }

    if score.abs() < cfg.momentum_score_threshold {
        return None;
    }

    let strength = score.abs() * conf.clamp(0.0, 1.0);
    if strength < cfg.momentum_score_threshold {
        return None;
    }

    if m.mode == Mode::Balance || !m.pos.is_balanced() {
        return None;
    }

    // Use actual window_s so taper behaves correctly even if window length differs.
    let tf = taper_factor(t_rem, window_s);
    let max_extra = (cfg.momentum_min_extra as f64
        + (cfg.momentum_base_extra - cfg.momentum_min_extra) as f64 * tf)
        .round() as i64;

    if m.momentum_used_extra >= max_extra {
        return None;
    }

    let eps = 0.15;
    if raw_score.abs() < eps {
        return None; 
    }

    // Make sure signal isn't choppy
    if raw_score.signum() != score.signum() {
        return None;
    }
    let side = momentum_side(score);

    let Some(ask) = m.book.implied_ask(side) else { return None; };
    if ask > cfg.max_buy_price_cents {
        return None;
    }

    if let Some(last) = last_taker(m, side) {
        if (now.duration_since(last).as_millis() as u64) < cfg.taker_cooldown_ms {
            return None;
        }
    }

    if let Some(_old_pc) = m.pos.pair_cost_cc() {
        let sim = m.pos.simulate_buy(side, ask, 1);
        if let Some(new_pc) = sim.pair_cost_cc() {
            if new_pc > cfg.safe_pair_cc.max(SAFE_PAIR_CC) {
                return None;
            }
        }
    }

    m.momentum_used_extra += 1;

    let client_order_id = uuid::Uuid::new_v4();
    m.orders.insert_pending(OrderRec {
        ticker: ticker.to_string(),
        side,
        price_cents: ask,
        qty: 1,
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
        qty: 1,
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

    let cap_target = cfg.target_pair_cc.max(TARGET_PAIR_CC);
    let cap_safe = cfg.safe_pair_cc.max(SAFE_PAIR_CC);
    let cap_balance = cfg.balance_pair_cc.max(DOLLAR_CC);

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
    let old_pc = m.pos.pair_cost_cc().unwrap_or(cap_safe);

    let p = if m.mode == Mode::Balance {
        // In Balance mode, prioritize hedging: allow up to balance cap.
        best_price_under_pair_cap(m, desired_side, top, min_price, cap_balance, false)?
    } else if old_pc <= cap_target {
        // Already under target: don't worsen, keep under target
        best_price_under_pair_cap(m, desired_side, top, min_price, cap_target, true)?
    } else {
        // Above target: allow any improvement (new_pc <= old_pc), even if still above target/safe
        best_price_under_pair_cap(m, desired_side, top, min_price, old_pc, true)?
    };

    if let Some(existing) = m.resting_hint(desired_side).as_ref().cloned() {
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

    let client_order_id = uuid::Uuid::new_v4();

    m.orders.insert_pending(OrderRec {
        ticker: ticker.to_string(),
        side: desired_side,
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

    let queue_ahead = match desired_side {
        Side::Yes => m.book.yes_bids[p as usize],
        Side::No => m.book.no_bids[p as usize],
    };
    *m.resting_hint_mut(desired_side) = Some(RestingHint {
        side: desired_side,
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
        side: desired_side,
        price_cents: p,
        qty: 1,
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

    // Use a stable per-market "window id":
    // - best is open_ts (unique per 15m market)
    // - fallback uses epoch-bucket id
    let wid = m.open_ts.unwrap_or_else(|| window_id(now_s, window_s));

    let prev_mode = m.mode;
    let is_new_window = m.window_id != wid;

    // Reset per-window counters.
    if is_new_window {
        m.window_id = wid;
        m.momentum_used_extra = 0;
        m.last_desired_side = None;
    }

    // Mode uses actual window_s (not cfg.window_s).
    m.mode = pick_mode(cfg, t_rem, window_s);
    // println!("Current Mode: {:#?}", m.mode);

    // ----------------------- DEBUG STUFF ------------------
        // ----- snapshot useful “price picture” -----
    let yes_bid = m.book.best_bid(Side::Yes);
    let yes_ask = m.book.implied_ask(Side::Yes);
    let no_bid  = m.book.best_bid(Side::No);
    let no_ask  = m.book.implied_ask(Side::No);

    // quantities at best bids
    let yes_bid_qty = yes_bid.map(|p| m.book.yes_bids[p as usize]).unwrap_or(0);
    let no_bid_qty  = no_bid .map(|p| m.book.no_bids [p as usize]).unwrap_or(0);

    // implied asks are on the opposite bid book
    let yes_ask_qty = no_bid_qty;
    let no_ask_qty  = yes_bid_qty;

    let yes_mid = match (yes_bid, yes_ask) {
        (Some(b), Some(a)) => Some((b as f64 + a as f64) / 2.0),
        _ => None,
    };
    let yes_spread = match (yes_bid, yes_ask) {
        (Some(b), Some(a)) => Some((a as i16) - (b as i16)),
        _ => None,
    };

    // Map “YES mid cents” into [-1,+1] just for comparison:
    // -1 = 0c, 0 = 50c, +1 = 100c
    let price_score = yes_mid.map(|mid| (mid / 50.0) - 1.0);

    // Raw instantaneous book imbalance (depth-based, not price-based)
    let book_raw_now = signal::raw_book_imbalance(cfg, &m.book);

    m.flow.decay_event_flows(cfg, now);
    // ----- compute score/conf (this mutates score_ema internally) -----
    let score_prev = m.flow.score_ema.value;
    let (score,raw_score, conf) = signal::combined_score(cfg, m);
    let last_seq = m.book.last_seq;

    // Values actually used by the scorer
    let book_ema  = m.flow.book_imb_ema.value;
    let trade_ema = m.flow.trade_flow_ema.value;
    let delta_ema = m.flow.delta_flow_ema.value;

    // ----- recompute combined_score internals for logging (close to exact) -----
    let now2 = Instant::now();

    let (depth_now, depth_mult) = if cfg.enable_depth_norm {
        let n = cfg.depth_norm_levels.max(1);
        let y = weighted_top_n_qty(&m.book.yes_bids, n);
        let nqty = weighted_top_n_qty(&m.book.no_bids, n);
        let depth = (y + nqty).max(1.0);

        let mult = (cfg.depth_full_weight_qty.max(1.0) / depth)
            .clamp(cfg.depth_norm_min_mult, cfg.depth_norm_max_mult);

        (depth, mult)
    } else {
        (0.0, 1.0)
    };

    // Trade stats (rate window)
    let trade_n = m.flow.trade_count_recent(cfg, now2);
    let trade_abs = m.flow.trade_abs_recent(cfg, now2) as f64;
    let trade_net = m.flow.trade_net_recent(cfg, now2) as f64;

    let trade_f_n   = clamp01(trade_n as f64 / (cfg.trade_full_weight_count.max(1) as f64));
    let trade_f_abs = clamp01((trade_abs * depth_mult) / (cfg.trade_full_weight_abs.max(1) as f64));
    let trade_coh   = clamp01(trade_net.abs() / trade_abs.max(1e-9));
    let trade_factor = (trade_f_n * trade_f_abs * trade_coh).powf(1.0 / 3.0);

    // Delta stats (rate window)
    let delta_n   = m.flow.delta_count_recent(cfg, now2) as f64;
    let delta_abs = m.flow.delta_abs_recent(cfg, now2) as f64;
    let delta_net = m.flow.delta_net_recent(cfg, now2) as f64;

    let delta_f_n   = clamp01(delta_n / (cfg.delta_full_weight_count.max(1) as f64));
    let delta_f_abs = clamp01((delta_abs * depth_mult) / (cfg.delta_full_weight_abs.max(1) as f64));
    let delta_coh   = clamp01(delta_net.abs() / delta_abs.max(1e-9));
    let delta_factor = (delta_f_n * delta_f_abs * delta_coh).powf(1.0 / 3.0);

    let w_book  = cfg.w_book;
    let w_trade = cfg.w_trade * trade_factor;
    let w_delta = cfg.w_delta * delta_factor;
    let w_sum   = (w_book + w_trade + w_delta).max(1e-9);

    let raw_combined = (w_book * book_ema + w_trade * trade_ema + w_delta * delta_ema) / w_sum;

    // Conf parts (match signal.rs)
    let activity = clamp01((w_trade + w_delta) / (cfg.w_trade + cfg.w_delta).max(1e-9));
    let consensus = clamp01(
        (w_book * sign01(book_ema) + w_trade * sign01(trade_ema) + w_delta * sign01(delta_ema)).abs() / w_sum
    );
    let strength = clamp01(score.abs() / cfg.score_full_conf_abs.max(1e-9));
    let conf_calc = clamp01(activity * (0.5 + 0.5 * consensus) * strength);
    
    // if let (Some(no_bid), Some(no_ask), Some(yes_bid), Some(yes_ask)) = (no_bid, no_ask, yes_bid, yes_ask) {
    // println!("Yes_bid: {}, Yes_ask: {}, No_bid: {}, No_ask: {}", yes_bid, yes_ask, no_bid, no_ask);
    // }

    // ----- main debug dump (keep it gated so it’s readable) -----
    // let verbose = is_new_window || prev_mode != m.mode || (conf > 0.1 && score.abs() > 0.1);

    // if verbose {
    //     debug!(
    //         ticker = %ticker,
    //         last_seq = m.book.last_seq,
    //         open_ts = ?m.open_ts, close_ts = ?m.close_ts,
    //         wid, is_new_window,
    //         window_s, t_rem,
    //         prev_mode = ?prev_mode, mode = ?m.mode,
    //         "decide: timing"
    //     );

    //     debug!(
    //         ticker = %ticker,
    //         yes_bid = ?yes_bid, yes_ask = ?yes_ask,
    //         no_bid = ?no_bid,  no_ask  = ?no_ask,
    //         yes_bid_qty, no_bid_qty, yes_ask_qty, no_ask_qty,
    //         yes_mid = ?yes_mid, yes_spread = ?yes_spread, price_score = ?price_score,
    //         "decide: top-of-book"
    //     );

    //     debug!(
    //         ticker = %ticker,
    //         book_raw_now,
    //         book_ema, trade_ema, delta_ema,
    //         raw_combined,
    //         score, conf,
    //         conf_calc,
    //         activity, consensus, strength,
    //         trade_n, trade_abs, trade_net, trade_factor,
    //         delta_n, delta_abs, delta_net, delta_factor,
    //         depth_now, depth_mult,
    //         yes_top = ?top_levels(&m.book.yes_bids, 5),
    //         no_top  = ?top_levels(&m.book.no_bids, 5),
    //         "decide: score/conf breakdown"
    //     );
    // }

    // ----------------------------------------------------
    let desired_side = if has_pair(m) {
        choose_working_side_simple(cfg, m, t_rem)
    } else {
        choose_bootstrap_side_simple(cfg, m)
    };
    m.last_desired_side = Some(desired_side);

    let strength = score.abs() * conf.clamp(0.0, 1.0);

    // tracing::debug!(
    //     ticker = %ticker,
    //     wid = wid,
    //     mode = ?m.mode,
    //     t_rem,
    //     window_s,
    //     score,
    //     conf,
    //     strength,
    //     desired_side = ?desired_side,
    //     imbr = m.pos.imbalance_ratio(),
    //     yes = m.pos.yes_qty,
    //     no = m.pos.no_qty,
    //     "decision signal summary"
    // );


    // 0) Cancel stale resting orders (but never churn fast).
    if let Some(cmd) = cancel_stale_if_needed(cfg, ticker, m, now) {
        return Some(cmd);
    }

    // 1) Opportunistic taker (cost-driven): if ask is cheap enough to improve/keep caps.
    if let Some(cmd) = maybe_opportunistic_taker(cfg, ticker, m, now, t_rem, desired_side) {
        return Some(cmd);
    }

    // 2) Momentum taker (flow-driven): strong score, balanced inventory, within safe cap.
    // if let Some(cmd) = maybe_momentum_taker(cfg, ticker, m, now, t_rem, window_s, raw_score, score, conf) {
    //     return Some(cmd);
    // }

    // 3) Maker quoting (resting) with churn control.
    maybe_maker_quote(cfg, ticker, m, now, t_rem, desired_side)
}
