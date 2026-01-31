use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::engine::signal;
use crate::state::orders::{OrderRec, OrderStatus};
use crate::state::ticker::{Market, Mode};
use crate::types::{ExecCommand, RestingHint, Side, Tif, SAFE_PAIR_CC, TARGET_PAIR_CC};

fn unix_now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
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

/// Decide which side we “prefer” to work right now:
/// - if we MUST balance => hedge side
/// - else if flow score is strong and we’re not too imbalanced => flow side
/// - else => hedge side
fn choose_working_side(cfg: &Config, m: &Market, t_rem: i64, score: f64) -> Side {
    let imbalance_cap = allowed_imbalance(cfg, t_rem);
    let imbr = m.pos.imbalance_ratio();

    let hedge_side = if m.pos.yes_qty <= m.pos.no_qty { Side::Yes } else { Side::No };
    let flow_side = momentum_side(score);

    let must_balance = m.mode == Mode::Balance || imbr > imbalance_cap;

    if must_balance {
        hedge_side
    } else if score.abs() >= cfg.momentum_score_threshold {
        flow_side
    } else {
        hedge_side
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
    score: f64,
) -> Option<ExecCommand> {
    let imbalance_cap = allowed_imbalance(cfg, t_rem);
    let must_balance = m.mode == Mode::Balance || m.pos.imbalance_ratio() > imbalance_cap;

    let desired = choose_working_side(cfg, m, t_rem, score);

    let mut best: Option<(Side, u8, i64)> = None;

    for side in [Side::Yes, Side::No] {
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

        if let Some(old_pc) = m.pos.pair_cost_cc() {
            let sim = m.pos.simulate_buy(side, ask, 1);
            let Some(new_pc) = sim.pair_cost_cc() else { continue; };

            if new_pc <= cfg.target_pair_cc.max(TARGET_PAIR_CC) {
                best = Some((side, ask, new_pc));
                continue;
            }

            let improve = old_pc - new_pc;
            if improve >= cfg.min_taker_improve_cc {
                best = Some((side, ask, new_pc));
            }
        } else {
            if side == desired {
                best = Some((side, ask, 0));
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
fn maybe_momentum_taker(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    window_s: i64,   // <-- NEW: use actual window length for taper
    score: f64,
) -> Option<ExecCommand> {
    if score.abs() < cfg.momentum_score_threshold {
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
    score: f64,
) -> Option<ExecCommand> {
    let desired_side = choose_working_side(cfg, m, t_rem, score);

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

    let top = top_maker_price(cfg, m, desired_side)?;
    let min_price = top.saturating_sub(cfg.maker_max_edge_cents);

    let p = if let Some(p1) = best_price_under_pair_cap(m, desired_side, top, min_price, cap_target, true) {
        p1
    } else if m.mode != Mode::Balance {
        best_price_under_pair_cap(m, desired_side, top, min_price, cap_safe, false)?
    } else {
        return None;
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
    });

    *m.resting_hint_mut(desired_side) = Some(RestingHint {
        side: desired_side,
        price_cents: p,
        created_at: now,
        cancel_requested_at: None,
        client_order_id,
        order_id: None,
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
    }

    // Mode uses actual window_s (not cfg.window_s).
    m.mode = pick_mode(cfg, t_rem, window_s);
    // println!("Current Mode: {:#?}", m.mode);

    // Score is independent of time logic.
    let (score, conf) = signal::combined_score(cfg, m);
    println!("Score, Conf {:#?}, {:#?}", score, conf);

    // log only when something meaningful changes
    // if is_new_window || m.mode != prev_mode || score.abs() >= cfg.momentum_score_threshold {
    //     tracing::info!(
    //         ticker = %ticker,
    //         wid = wid,
    //         window_s = window_s,
    //         t_rem = t_rem,
    //         mode = ?m.mode,
    //         book_ema = m.flow.book_imb_ema.value,
    //         trade_ema = m.flow.trade_flow_ema.value,
    //         delta_ema = m.flow.delta_flow_ema.value,
    //         score = score,
    //         conf = conf,
    //         "decision inputs"
    //     );
    // }

    // 0) Cancel stale resting orders (but never churn fast).
    if let Some(cmd) = cancel_stale_if_needed(cfg, ticker, m, now) {
        return Some(cmd);
    }

    // 1) Opportunistic taker (cost-driven): if ask is cheap enough to improve/keep caps.
    if let Some(cmd) = maybe_opportunistic_taker(cfg, ticker, m, now, t_rem, score) {
        return Some(cmd);
    }

    // 2) Momentum taker (flow-driven): strong score, balanced inventory, within safe cap.
    if let Some(cmd) = maybe_momentum_taker(cfg, ticker, m, now, t_rem, window_s, score) {
        return Some(cmd);
    }

    // 3) Maker quoting (resting) with churn control.
    maybe_maker_quote(cfg, ticker, m, now, t_rem, score)
}
