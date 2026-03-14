use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::state::coinbase::{CoinbaseSignal, CoinbaseSnapshot, SignalRegime};
use crate::state::orders::{OrderRec, OrderStatus};
use crate::state::position::Position;
use crate::state::ticker::{Market, Mode};
use crate::types::{CC_PER_CENT, ExecCommand, RestingHint, Side, Tif};

const DOLLAR_CC: i64 = 100 * CC_PER_CENT;
const MAX_CAP_CC: i64 = 200 * CC_PER_CENT;
const NO_ORDER_LOG_REPEAT_SECS: u64 = 5;
const COINBASE_SIGNAL_LOG_REPEAT_MS: u64 = 5_000;
const PAIR_OPEN_LOG_REPEAT_MS: u64 = 5_000;
const MIN_HEDGE_FLOOR_IMPROVE_CC: i64 = 500;
const PAIR_REPAIR_HYST_CC: i64 = 50;
const COINBASE_SIGNAL_WEIGHT: f64 = 0.35;
const MAX_OPEN_DISLOCATION_CENTS: f64 = 4.0;
const MAX_PAIR_OPEN_BACKOFF_CENTS: u8 = 1;

fn clear_no_order_reason(m: &mut Market) {
    m.last_no_order_reason = None;
    m.last_no_order_reason_ts = None;
}

fn log_no_order_reason(
    ticker: &str,
    m: &mut Market,
    reason: &'static str,
    t_rem: i64,
    signal: Option<&CoinbaseSignal>,
    gap: i64,
    allowed_gap: i64,
) {
    let now = Instant::now();
    if m.last_no_order_reason == Some(reason) {
        if let Some(ts) = m.last_no_order_reason_ts {
            if now.duration_since(ts).as_secs() < NO_ORDER_LOG_REPEAT_SECS {
                return;
            }
        }
    }
    m.last_no_order_reason = Some(reason);
    m.last_no_order_reason_ts = Some(now);

    tracing::info!(
        ticker = %ticker,
        reason,
        mode = ?m.mode,
        t_rem,
        yes_qty = m.pos.yes_qty,
        no_qty = m.pos.no_qty,
        locked_floor_cc = m.pos.locked_floor_cc(),
        pair_cost_cc = ?m.pos.pair_cost_cc(),
        gap,
        allowed_gap,
        resting_yes = m.resting_yes.is_some(),
        resting_no = m.resting_no.is_some(),
        best_yes_bid = ?m.book.best_bid(Side::Yes),
        best_yes_ask = ?m.book.implied_ask(Side::Yes),
        best_no_bid = ?m.book.best_bid(Side::No),
        best_no_ask = ?m.book.implied_ask(Side::No),
        signal_regime = ?signal.map(|s| s.regime),
        fair_yes_cents = ?signal.map(|s| s.fair_yes_cents),
        trend_z = ?signal.map(|s| s.trend_z),
        signal_age_ms = ?signal.map(|s| s.age_ms),
        "no order placed"
    );
}

fn log_coinbase_fair_value(
    ticker: &str,
    m: &mut Market,
    now: Instant,
    signal: &CoinbaseSignal,
    strike_price: f64,
    t_rem: i64,
) {
    let due = match m.last_signal_log_ts {
        Some(ts) => now.duration_since(ts).as_millis() as u64 >= COINBASE_SIGNAL_LOG_REPEAT_MS,
        None => true,
    };
    let fair_changed = m.last_signal_log_fair_cents != Some(signal.fair_yes_cents);
    if !due && !fair_changed {
        return;
    }

    m.last_signal_log_ts = Some(now);
    m.last_signal_log_fair_cents = Some(signal.fair_yes_cents);

    tracing::info!(
        ticker = %ticker,
        t_rem,
        strike_price,
        coinbase_price = signal.price,
        microprice = signal.microprice,
        ema_fast = signal.ema_fast,
        ema_slow = signal.ema_slow,
        fair_yes = signal.fair_yes,
        fair_yes_cents = signal.fair_yes_cents,
        fair_no_cents = 100u8.saturating_sub(signal.fair_yes_cents),
        distance_usd = signal.distance_usd,
        sigma_usd = signal.sigma_usd,
        vol_ema_usd = signal.vol_ema_usd,
        trend_z = signal.trend_z,
        regime = ?signal.regime,
        realized_final_avg = ?signal.realized_final_avg,
        required_remaining_avg = ?signal.required_remaining_avg,
        "coinbase fair value"
    );
}

fn log_pair_open_quote_prices(
    ticker: &str,
    m: &mut Market,
    now: Instant,
    yes_price: u8,
    no_price: u8,
) {
    let due = match m.last_pair_open_log_ts {
        Some(ts) => now.duration_since(ts).as_millis() as u64 >= PAIR_OPEN_LOG_REPEAT_MS,
        None => true,
    };
    if !due {
        return;
    }

    m.last_pair_open_log_ts = Some(now);

    tracing::info!(
        ticker = %ticker,
        yes_price,
        no_price,
        current_pair_cost_cc = ?m.pos.pair_cost_cc(),
        current_locked_floor_cc = m.pos.locked_floor_cc(),
        "pair open quote prices"
    );
}

fn unix_now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn total_qty(m: &Market) -> i64 {
    (m.pos.yes_qty.max(0) + m.pos.no_qty.max(0)).max(1)
}

fn unhedged_qty(m: &Market) -> i64 {
    (m.pos.yes_qty - m.pos.no_qty).abs().max(0)
}

fn qty_for(pos: &Position, side: Side) -> i64 {
    match side {
        Side::Yes => pos.yes_qty,
        Side::No => pos.no_qty,
    }
}

fn has_pair(m: &Market) -> bool {
    m.pos.yes_qty > 0 && m.pos.no_qty > 0
}

fn allowed_unhedged_qty(cfg: &Config, m: &Market) -> i64 {
    if m.mode == Mode::Balance {
        cfg.max_unhedged_qty_late.max(0)
    } else {
        cfg.max_unhedged_qty_early.max(0)
    }
}

fn needs_hedge_only(gap: i64, allowed_gap: i64) -> bool {
    gap > 0 && gap >= allowed_gap.max(1)
}

fn strict_balance_cap_cc(cfg: &Config) -> i64 {
    cfg.balance_pair_cc.clamp(0, DOLLAR_CC)
}

fn closeout_cap_cc(cfg: &Config, t_rem: i64) -> i64 {
    if t_rem <= cfg.taker_desperate_s {
        cfg.final_balance_pair_cc.clamp(0, MAX_CAP_CC)
    } else {
        cfg.balance_pair_cc.clamp(0, DOLLAR_CC)
    }
}

fn time_remaining_s(now_s: i64, window_s: i64) -> i64 {
    let w = window_s.max(1);
    let start = (now_s / w) * w;
    let end = start + w;
    (end - now_s).max(0)
}

fn effective_window_s(cfg: &Config, m: &Market) -> i64 {
    match (m.open_ts, m.close_ts) {
        (Some(o), Some(c)) if c > o => (c - o).max(1),
        _ => cfg.window_s.max(1),
    }
}

fn effective_time_remaining_s(m: &Market, now_s: i64, window_s: i64) -> i64 {
    if let Some(c) = m.close_ts {
        return (c - now_s).max(0);
    }
    if let Some(o) = m.open_ts {
        return ((o + window_s) - now_s).max(0);
    }
    time_remaining_s(now_s, window_s)
}

fn pick_mode(cfg: &Config, t_rem: i64, window_s: i64) -> Mode {
    if t_rem <= cfg.balance_s {
        Mode::Balance
    } else if t_rem > (window_s - cfg.accumulate_s) {
        Mode::Accumulate
    } else {
        Mode::Hedge
    }
}

fn hedge_side(m: &Market) -> Side {
    if m.pos.yes_qty < m.pos.no_qty {
        Side::Yes
    } else if m.pos.no_qty < m.pos.yes_qty {
        Side::No
    } else {
        match (m.book.implied_ask(Side::Yes), m.book.implied_ask(Side::No)) {
            (Some(ay), Some(an)) => {
                if ay <= an {
                    Side::Yes
                } else {
                    Side::No
                }
            }
            (Some(_), None) => Side::Yes,
            (None, Some(_)) => Side::No,
            _ => Side::Yes,
        }
    }
}

fn stage_place_order(
    ticker: &str,
    m: &mut Market,
    side: Side,
    price_cents: u8,
    qty: u64,
    tif: Tif,
    post_only: bool,
) -> (uuid::Uuid, ExecCommand) {
    let client_order_id = uuid::Uuid::new_v4();

    m.orders.insert_pending(OrderRec {
        qty,
        order_id: None,
        client_order_id,
        status: OrderStatus::PendingAck,
        filled_qty: 0,
    });

    let cmd = ExecCommand::PlaceOrder {
        ticker: ticker.to_string(),
        side,
        price_cents,
        qty,
        tif,
        post_only,
        client_order_id,
    };

    (client_order_id, cmd)
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

fn desired_buy_qty(cfg: &Config, m: &Market, side: Side, t_rem: i64, window_s: i64) -> u64 {
    let yes = m.pos.yes_qty.max(0) as i64;
    let no = m.pos.no_qty.max(0) as i64;
    let (my, other) = match side {
        Side::Yes => (yes, no),
        Side::No => (no, yes),
    };

    let gap = (other - my).max(0);
    if gap <= 0 {
        return 1;
    }

    let tf = (t_rem as f64 / window_s.max(1) as f64).clamp(0.0, 1.0);
    let urgency = (1.0 - tf).clamp(0.0, 1.0);
    let mut frac = cfg.catchup_aggressiveness * (0.35 + 0.65 * urgency);
    if m.mode == Mode::Balance {
        frac *= 1.0 + cfg.catchup_balance_boost;
    }

    let min_short = if has_pair(m) {
        cfg.short_side_min_order_qty.max(1) as i64
    } else {
        1
    };

    let q = ((gap as f64) * frac).ceil() as i64;
    q.max(min_short)
        .clamp(1, gap.max(1))
        .min(cfg.max_order_qty as i64) as u64
}

fn drift_threshold_cents(cfg: &Config, m: &Market, side: Side) -> u8 {
    if side == hedge_side(m) {
        cfg.cancel_drift_cents_hedge.max(1)
    } else {
        cfg.cancel_drift_cents.max(1)
    }
}

fn top_maker_price(cfg: &Config, m: &Market, side: Side) -> Option<u8> {
    let improve = if m.mode == Mode::Balance {
        cfg.maker_improve_tick_balance
    } else {
        cfg.maker_improve_tick
    };

    let ask_limit = match m.book.implied_ask(side) {
        Some(0) => return None,
        Some(ask) => Some(ask.saturating_sub(1)),
        None => None,
    };

    let best_bid = m.book.best_bid(side);
    let mut p = match (best_bid, ask_limit) {
        (Some(best), Some(limit)) => best.saturating_add(improve).min(limit),
        (Some(best), None) => best.saturating_add(improve),
        (None, Some(limit)) => limit,
        (None, None) => return None,
    };

    let gap = unhedged_qty(m);
    if side == hedge_side(m) && gap >= cfg.hedge_force_ask_minus_one_gap.max(1) {
        if let Some(ask) = m.book.implied_ask(side) {
            if ask > 0 {
                p = p.max(ask.saturating_sub(1));
            }
        }
    }

    Some(p.min(cfg.max_buy_price_cents))
}

fn cancel_stale_if_needed(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
) -> Option<ExecCommand> {
    for side in Side::ALL {
        let Some(h) = m.resting_hint(side).as_ref().cloned() else {
            continue;
        };
        let Some(order_id) = h.order_id.clone() else {
            continue;
        };

        let age_ms = now.duration_since(h.created_at).as_millis() as u64;
        if age_ms < cfg.min_resting_life_ms {
            continue;
        }

        if let Some(t0) = h.cancel_requested_at {
            let since = now.duration_since(t0).as_millis() as u64;
            if since < cfg.cancel_retry_ms {
                continue;
            }
        }

        if age_ms >= cfg.cancel_stale_ms {
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

fn cancel_side_force(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    side: Side,
) -> Option<ExecCommand> {
    let Some(h) = m.resting_hint(side).as_ref().cloned() else {
        return None;
    };

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

    if let Some(hm) = m.resting_hint_mut(side).as_mut() {
        hm.cancel_requested_at = Some(now);
    }

    let Some(order_id) = h.order_id.clone() else {
        return None;
    };

    Some(ExecCommand::CancelOrder {
        ticker: ticker.to_string(),
        order_id,
    })
}

fn place_or_manage_resting(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    side: Side,
    p: u8,
    qty: u64,
    only_reprice_if_more_aggressive: bool,
) -> Option<ExecCommand> {
    if let Some(existing) = m.resting_hint(side).as_ref().cloned() {
        let existing_remaining = m
            .orders
            .by_client
            .get(&existing.client_order_id)
            .map(|r| r.qty.saturating_sub(r.filled_qty))
            .unwrap_or(1);

        let want_upsize = qty > existing_remaining;
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
        let drift_threshold = drift_threshold_cents(cfg, m, side);
        let more_aggressive = p > existing.price_cents;
        let drift_triggers_cancel = if only_reprice_if_more_aggressive {
            more_aggressive && drift >= drift_threshold
        } else {
            drift >= drift_threshold
        };

        if drift_triggers_cancel || want_upsize {
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

    let (client_order_id, cmd) = stage_place_order(ticker, m, side, p, qty.max(1), Tif::Gtc, true);
    let queue_ahead = match side {
        Side::Yes => m.book.yes_bids[p as usize],
        Side::No => m.book.no_bids[p as usize],
    };
    *m.resting_hint_mut(side) = Some(RestingHint {
        price_cents: p,
        created_at: now,
        cancel_requested_at: None,
        client_order_id,
        order_id: None,
        queue_ahead,
    });

    Some(cmd)
}

fn inventory_shift_cents(cfg: &Config, m: &Market) -> i32 {
    let raw = (m.pos.no_qty - m.pos.yes_qty) * cfg.inventory_skew_per_contract_cents as i64;
    raw.clamp(
        -(cfg.inventory_skew_max_cents as i64),
        cfg.inventory_skew_max_cents as i64,
    ) as i32
}

fn vol_extra_cents(cfg: &Config, signal: &CoinbaseSignal) -> i32 {
    if cfg.quote_vol_per_extra_cent_usd <= 0.0 {
        return 0;
    }
    ((signal.vol_ema_usd / cfg.quote_vol_per_extra_cent_usd).floor() as i32)
        .clamp(0, cfg.quote_max_vol_extra_cents as i32)
}

fn kalshi_mid_yes_cents(m: &Market) -> Option<f64> {
    Some((m.book.best_bid(Side::Yes)? as f64 + m.book.implied_ask(Side::Yes)? as f64) / 2.0)
}

fn anchored_fair_yes_cents(m: &Market, signal: &CoinbaseSignal) -> u8 {
    let Some(kalshi_mid) = kalshi_mid_yes_cents(m) else {
        return signal.fair_yes_cents;
    };

    let raw = signal.fair_yes_cents as f64;
    let blended = kalshi_mid + COINBASE_SIGNAL_WEIGHT * (raw - kalshi_mid);
    blended.round().clamp(1.0, 99.0) as u8
}

fn can_open_pairs(cfg: &Config, m: &Market, signal: &CoinbaseSignal, t_rem: i64) -> bool {
    if t_rem <= cfg.no_new_imbalance_s {
        return false;
    }

    let Some(kalshi_mid) = kalshi_mid_yes_cents(m) else {
        return false;
    };

    // Blend Coinbase back toward the live Kalshi mid so aligned drift/extreme
    // states can still open pairs without reacting to transient dislocations.
    let fair = anchored_fair_yes_cents(m, signal) as f64;
    let dislocation = (fair - kalshi_mid).abs();

    matches!(
        signal.regime,
        SignalRegime::TwoSided | SignalRegime::DriftUp | SignalRegime::DriftDown
    ) || dislocation <= MAX_OPEN_DISLOCATION_CENTS
}

fn desired_maker_quote(
    cfg: &Config,
    m: &Market,
    side: Side,
    signal: &CoinbaseSignal,
) -> Option<u8> {
    let mut top = top_maker_price(cfg, m, side)?;
    let gap = unhedged_qty(m);
    if side == hedge_side(m) && gap >= cfg.hedge_force_ask_minus_one_gap.max(1) {
        if let Some(ask) = m.book.implied_ask(side) {
            if ask > 0 {
                top = top.max(ask.saturating_sub(1));
            }
        }
    }

    // Use the anchored fair here too so maker quotes stay close to the actual
    // Kalshi book instead of chasing raw Coinbase fair when the venues diverge.
    let fair_yes_cents = anchored_fair_yes_cents(m, signal);
    let mut center = fair_yes_cents as i32 + inventory_shift_cents(cfg, m);
    center = center.clamp(1, cfg.max_buy_price_cents as i32);

    let half = cfg.quote_base_halfspread_cents as i32 + vol_extra_cents(cfg, signal);
    let mut target = match side {
        Side::Yes => center - half,
        Side::No => 100 - (center + half),
    };

    if side == hedge_side(m) && gap > 0 {
        target += cfg.hedge_quote_boost_cents as i32;
    }

    target = target.clamp(1, cfg.max_buy_price_cents as i32);
    let target_u = target as u8;

    let edge_band = if m.mode == Mode::Balance && !m.pos.is_balanced() {
        cfg.maker_max_edge_cents_balance
    } else {
        cfg.maker_max_edge_cents
    };
    let min_price = top.saturating_sub(edge_band);
    if target_u < min_price {
        return None;
    }

    Some(target_u.min(top))
}

fn plausible_missing_price_cents(cfg: &Config, m: &Market, missing_side: Side) -> Option<u8> {
    top_maker_price(cfg, m, missing_side).or_else(|| m.book.best_bid(missing_side))
}

fn admission_ok(cfg: &Config, m: &Market, side: Side, price_cents: u8, qty: u64) -> bool {
    let sim = m.pos.simulate_buy(side, price_cents, qty as i64);
    let old_gap = unhedged_qty(m);
    let new_gap = (sim.yes_qty - sim.no_qty).abs();
    let old_floor = m.pos.locked_floor_cc();
    let new_floor = sim.locked_floor_cc();
    let old_pc = m.pos.pair_cost_cc();
    let new_pc = sim.pair_cost_cc();

    // When a fill reduces an imbalance, judge it on guaranteed-floor repair
    // rather than demanding the average pair cost also improve.
    if new_gap < old_gap {
        if new_floor >= cfg.locked_floor_buffer_cc {
            return true;
        }

        if old_floor < 0 && new_floor >= old_floor + MIN_HEDGE_FLOOR_IMPROVE_CC {
            return true;
        }

        return false;
    }

    // Opening fresh same-side risk stays strict on pair cost and catch-up math.
    if let Some(pc) = new_pc {
        if pc > cfg.safe_pair_cc {
            return false;
        }
        if let Some(old) = old_pc {
            if pc > old {
                return false;
            }
        }
    }
    if new_floor >= cfg.locked_floor_buffer_cc {
        return true;
    }

    let missing = if qty_for(&sim, Side::Yes) > qty_for(&sim, Side::No) {
        Side::No
    } else if qty_for(&sim, Side::No) > qty_for(&sim, Side::Yes) {
        Side::Yes
    } else {
        return false;
    };
    let Some(max_avg_cc) = sim.max_avg_price_to_balance_cc(missing, cfg.locked_floor_buffer_cc)
    else {
        return false;
    };
    let plausible_cc = plausible_missing_price_cents(cfg, m, missing)
        .unwrap_or_else(|| m.book.implied_ask(missing).unwrap_or(99)) as i64
        * CC_PER_CENT;
    max_avg_cc >= plausible_cc - cfg.catchup_plausibility_buffer_cents as i64 * CC_PER_CENT
}

fn two_sided_pair_cost_ok(cfg: &Config, yes_price: u8, no_price: u8) -> bool {
    (yes_price as i64 + no_price as i64) * CC_PER_CENT <= cfg.market_entry_pair_cost_cc
}

fn is_balanced_but_bad(cfg: &Config, m: &Market) -> bool {
    m.pos.is_balanced()
        && (m.pos.yes_qty > 0 || m.pos.no_qty > 0)
        && (m.pos.locked_floor_cc() < cfg.locked_floor_buffer_cc
            || m.pos
                .pair_cost_cc()
                .is_some_and(|pc| pc > cfg.target_pair_cc + PAIR_REPAIR_HYST_CC))
}

fn repair_side_for(regime: SignalRegime) -> Option<Side> {
    match regime {
        SignalRegime::DriftDown | SignalRegime::ExtremeDown | SignalRegime::PinnedDown => {
            Some(Side::Yes)
        }
        SignalRegime::DriftUp | SignalRegime::ExtremeUp | SignalRegime::PinnedUp => Some(Side::No),
        SignalRegime::TwoSided => None,
    }
}

fn repair_quote_improves_book(cfg: &Config, m: &Market, side: Side, price_cents: u8) -> bool {
    let Some(current_pc) = m.pos.pair_cost_cc() else {
        return false;
    };
    let current_locked_floor = m.pos.locked_floor_cc();
    let missing = side.other();
    let Some(missing_price) =
        plausible_missing_price_cents(cfg, m, missing).or_else(|| m.book.implied_ask(missing))
    else {
        return false;
    };

    let repaired = m
        .pos
        .simulate_buy(side, price_cents, 1)
        .simulate_buy(missing, missing_price, 1);

    repaired.locked_floor_cc() > current_locked_floor
        || repaired.pair_cost_cc().is_some_and(|pc| pc < current_pc)
}

fn maybe_signal_maker_quote(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    window_s: i64,
    side: Side,
    signal: &CoinbaseSignal,
) -> Option<ExecCommand> {
    let mut qty = desired_buy_qty(cfg, m, side, t_rem, window_s).max(1);
    let mut price = desired_maker_quote(cfg, m, side, signal)?;

    if !admission_ok(cfg, m, side, price, qty) {
        qty = 1;
        if !admission_ok(cfg, m, side, price, qty) {
            for p in (1..price).rev() {
                if admission_ok(cfg, m, side, p, 1) {
                    price = p;
                    break;
                }
            }
            if !admission_ok(cfg, m, side, price, 1) {
                return None;
            }
            qty = 1;
        }
    }

    let sticky = side == hedge_side(m) && !m.pos.is_balanced();
    place_or_manage_resting(cfg, ticker, m, now, side, price, qty, sticky)
}

enum PairOpenResult {
    Command(ExecCommand),
    Working,
    Unavailable,
}

fn has_active_resting_pair(m: &Market) -> bool {
    m.pos.yes_qty == 0
        && m.pos.no_qty == 0
        && m.resting_yes
            .as_ref()
            .is_some_and(|h| h.cancel_requested_at.is_none())
        && m.resting_no
            .as_ref()
            .is_some_and(|h| h.cancel_requested_at.is_none())
}

fn pair_open_is_working(m: &Market, yes_price: u8, no_price: u8) -> bool {
    m.resting_yes
        .as_ref()
        .is_some_and(|h| h.price_cents == yes_price && h.cancel_requested_at.is_none())
        && m.resting_no
            .as_ref()
            .is_some_and(|h| h.price_cents == no_price && h.cancel_requested_at.is_none())
}

fn pair_open_ok(cfg: &Config, m: &Market, yes_price: u8, no_price: u8, qty: u64) -> bool {
    let sim = m
        .pos
        .simulate_buy(Side::Yes, yes_price, qty as i64)
        .simulate_buy(Side::No, no_price, qty as i64);

    let cur_floor = m.pos.locked_floor_cc();
    let new_floor = sim.locked_floor_cc();
    if new_floor < cur_floor {
        return false;
    }

    if let (Some(cur_pc), Some(new_pc)) = (m.pos.pair_cost_cc(), sim.pair_cost_cc()) {
        if new_pc <= cur_pc {
            return true;
        }
    }

    two_sided_pair_cost_ok(cfg, yes_price, no_price)
}

fn pair_open_prices(cfg: &Config, m: &Market, signal: &CoinbaseSignal) -> Option<(u8, u8)> {
    let top_yes = top_maker_price(cfg, m, Side::Yes)?;
    let top_no = top_maker_price(cfg, m, Side::No)?;

    // Balanced pair opening should start by joining the best affordable pair
    // instead of forcing each leg through single-leg rescue pricing.
    if pair_open_ok(cfg, m, top_yes, top_no, 1) {
        return Some((top_yes, top_no));
    }

    let fair_yes = desired_maker_quote(cfg, m, Side::Yes, signal)
        .unwrap_or(top_yes)
        .max(top_yes.saturating_sub(MAX_PAIR_OPEN_BACKOFF_CENTS));
    let fair_no = desired_maker_quote(cfg, m, Side::No, signal)
        .unwrap_or(top_no)
        .max(top_no.saturating_sub(MAX_PAIR_OPEN_BACKOFF_CENTS));

    if pair_open_ok(cfg, m, fair_yes, fair_no, 1) {
        return Some((fair_yes, fair_no));
    }

    None
}

fn maybe_open_pair_quote(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    signal: &CoinbaseSignal,
) -> PairOpenResult {
    let Some((yes_price, no_price)) = pair_open_prices(cfg, m, signal) else {
        return PairOpenResult::Unavailable;
    };

    log_pair_open_quote_prices(ticker, m, now, yes_price, no_price);

    if let Some(cmd) = place_or_manage_resting(cfg, ticker, m, now, Side::Yes, yes_price, 1, false)
    {
        return PairOpenResult::Command(cmd);
    }
    if let Some(cmd) = place_or_manage_resting(cfg, ticker, m, now, Side::No, no_price, 1, false) {
        return PairOpenResult::Command(cmd);
    }

    if pair_open_is_working(m, yes_price, no_price) {
        return PairOpenResult::Working;
    }

    PairOpenResult::Unavailable
}

fn maybe_repair_quote(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    window_s: i64,
    side: Side,
    signal: &CoinbaseSignal,
) -> Option<ExecCommand> {
    let price = desired_maker_quote(cfg, m, side, signal)?;
    if !repair_quote_improves_book(cfg, m, side, price) {
        return None;
    }
    maybe_signal_maker_quote(cfg, ticker, m, now, t_rem, window_s, side, signal)
}

fn maybe_balance_ioc(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
    t_rem: i64,
    hedge: Side,
    gap: i64,
) -> Option<ExecCommand> {
    if gap <= 0 {
        return None;
    }

    if t_rem > cfg.taker_desperate_s {
        if let Some(h) = m.resting_hint(hedge).as_ref() {
            let age_ms = now.duration_since(h.created_at).as_millis() as u64;
            if age_ms < cfg.maker_first_ms {
                return None;
            }
        }
    }

    let Some(ask) = m.book.implied_ask(hedge) else {
        return None;
    };
    if ask > cfg.max_buy_price_cents {
        return None;
    }

    if let Some(last) = last_taker(m, hedge) {
        if (now.duration_since(last).as_millis() as u64) < cfg.taker_cooldown_ms {
            return None;
        }
    }

    let qty = (gap as u64).max(1).min(cfg.max_order_qty.max(1));
    if !admission_ok(cfg, m, hedge, ask, qty) {
        return None;
    }

    let sim = m.pos.simulate_buy(hedge, ask, qty as i64);
    if let Some(new_pc) = sim.pair_cost_cc() {
        if new_pc > closeout_cap_cc(cfg, t_rem) {
            return None;
        }
    }

    let (_client_order_id, cmd) = stage_place_order(ticker, m, hedge, ask, qty, Tif::Ioc, false);
    set_last_taker(m, hedge, now);
    Some(cmd)
}

fn should_freeze_trading(cfg: &Config, m: &Market, t_rem: i64) -> bool {
    if cfg.freeze_if_balanced_s <= 0 || t_rem > cfg.freeze_if_balanced_s {
        return false;
    }
    if !m.pos.is_balanced() {
        return false;
    }
    m.pos.locked_floor_cc() >= cfg.locked_floor_buffer_cc
        || m.pos
            .pair_cost_cc()
            .is_some_and(|pc| pc <= cfg.target_pair_cc.min(strict_balance_cap_cc(cfg)))
}

fn cancel_all_if_any(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    now: Instant,
) -> Option<ExecCommand> {
    if let Some(cmd) = cancel_side_force(cfg, ticker, m, now, Side::Yes) {
        return Some(cmd);
    }
    if let Some(cmd) = cancel_side_force(cfg, ticker, m, now, Side::No) {
        return Some(cmd);
    }
    None
}

pub fn decide(
    cfg: &Config,
    ticker: &str,
    m: &mut Market,
    coinbase: Option<&CoinbaseSnapshot>,
) -> Option<ExecCommand> {
    let now_s = unix_now_s();
    let now = Instant::now();
    let mut t_rem = 0;
    let mut gap = unhedged_qty(m);
    let mut allowed_gap = allowed_unhedged_qty(cfg, m);

    if let Some(close_ts) = m.close_ts {
        if now_s >= close_ts {
            log_no_order_reason(ticker, m, "market_closed", t_rem, None, gap, allowed_gap);
            return None;
        }
    }

    let window_s = effective_window_s(cfg, m);
    t_rem = effective_time_remaining_s(m, now_s, window_s);
    m.mode = pick_mode(cfg, t_rem, window_s);
    gap = unhedged_qty(m);
    allowed_gap = allowed_unhedged_qty(cfg, m);

    if should_freeze_trading(cfg, m, t_rem) {
        if let Some(cmd) = cancel_all_if_any(cfg, ticker, m, now) {
            clear_no_order_reason(m);
            return Some(cmd);
        }
        log_no_order_reason(ticker, m, "freeze_balanced", t_rem, None, gap, allowed_gap);
        return None;
    }

    let Some(strike_price) = m.strike_price else {
        if let Some(cmd) = cancel_all_if_any(cfg, ticker, m, now) {
            clear_no_order_reason(m);
            return Some(cmd);
        }
        log_no_order_reason(
            ticker,
            m,
            "waiting_strike_price",
            t_rem,
            None,
            gap,
            allowed_gap,
        );
        return None;
    };
    let Some(coinbase) = coinbase else {
        if let Some(cmd) = cancel_all_if_any(cfg, ticker, m, now) {
            clear_no_order_reason(m);
            return Some(cmd);
        }
        log_no_order_reason(
            ticker,
            m,
            "waiting_coinbase_snapshot",
            t_rem,
            None,
            gap,
            allowed_gap,
        );
        return None;
    };
    let Some(signal) = coinbase.build_signal(cfg, strike_price, m.close_ts, t_rem) else {
        if let Some(cmd) = cancel_all_if_any(cfg, ticker, m, now) {
            clear_no_order_reason(m);
            return Some(cmd);
        }
        log_no_order_reason(
            ticker,
            m,
            "coinbase_signal_unavailable",
            t_rem,
            None,
            gap,
            allowed_gap,
        );
        return None;
    };

    tracing::trace!(
        ticker = ticker,
        fair_yes = signal.fair_yes,
        fair_yes_cents = signal.fair_yes_cents,
        price = signal.price,
        microprice = signal.microprice,
        ema_fast = signal.ema_fast,
        ema_slow = signal.ema_slow,
        vol_ema_usd = signal.vol_ema_usd,
        distance_usd = signal.distance_usd,
        sigma_usd = signal.sigma_usd,
        trend_z = signal.trend_z,
        age_ms = signal.age_ms,
        regime = ?signal.regime,
        realized_final_avg = ?signal.realized_final_avg,
        required_remaining_avg = ?signal.required_remaining_avg,
        total_qty = total_qty(m),
        "coinbase strategy signal"
    );
    log_coinbase_fair_value(ticker, m, now, &signal, strike_price, t_rem);

    let balanced_but_bad = is_balanced_but_bad(cfg, m);
    let repair_side = repair_side_for(signal.regime);
    let resting_pair_open = has_active_resting_pair(m);

    if let Some(cmd) = cancel_stale_if_needed(cfg, ticker, m, now) {
        clear_no_order_reason(m);
        return Some(cmd);
    }

    if let Some(vulnerable) = coinbase.cancel_vulnerable_side(&signal, cfg) {
        let skip_cancel_for_repair =
            balanced_but_bad && gap == 0 && repair_side == Some(vulnerable);
        if !skip_cancel_for_repair && !resting_pair_open {
            if let Some(cmd) = cancel_side_force(cfg, ticker, m, now, vulnerable) {
                clear_no_order_reason(m);
                return Some(cmd);
            }
        }
    }

    gap = unhedged_qty(m);
    allowed_gap = allowed_unhedged_qty(cfg, m);
    let hedge = hedge_side(m);
    let strong = hedge.other();
    let no_new_imbalance = t_rem <= cfg.no_new_imbalance_s;
    let open_pairs_allowed = can_open_pairs(cfg, m, &signal, t_rem);

    if (!open_pairs_allowed && gap == 0)
        || (m.mode == Mode::Balance && no_new_imbalance && m.pos.is_balanced())
    {
        if balanced_but_bad && !no_new_imbalance {
            if let Some(repair_side) = repair_side {
                if let Some(cmd) =
                    maybe_repair_quote(cfg, ticker, m, now, t_rem, window_s, repair_side, &signal)
                {
                    clear_no_order_reason(m);
                    return Some(cmd);
                }
            }
            log_no_order_reason(
                ticker,
                m,
                "balanced_bad_no_repair_quote",
                t_rem,
                Some(&signal),
                gap,
                allowed_gap,
            );
            return None;
        }

        if let Some(cmd) = cancel_all_if_any(cfg, ticker, m, now) {
            clear_no_order_reason(m);
            return Some(cmd);
        }
        let reason = if m.mode == Mode::Balance && no_new_imbalance && m.pos.is_balanced() {
            "balanced_endgame_hold"
        } else if no_new_imbalance {
            "endgame_no_new_imbalance"
        } else {
            "regime_blocks_opening"
        };
        log_no_order_reason(ticker, m, reason, t_rem, Some(&signal), gap, allowed_gap);
        return None;
    }

    if needs_hedge_only(gap, allowed_gap) || !open_pairs_allowed {
        if let Some(cmd) = cancel_side_force(cfg, ticker, m, now, strong) {
            clear_no_order_reason(m);
            return Some(cmd);
        }

        let force_ioc = m.mode == Mode::Balance
            && gap > 0
            && (t_rem <= cfg.taker_desperate_s || gap >= cfg.taker_force_gap.max(1));
        if force_ioc {
            if let Some(cmd) = maybe_balance_ioc(cfg, ticker, m, now, t_rem, hedge, gap) {
                clear_no_order_reason(m);
                return Some(cmd);
            }
        }

        if gap > 0 {
            if let Some(cmd) =
                maybe_signal_maker_quote(cfg, ticker, m, now, t_rem, window_s, hedge, &signal)
            {
                clear_no_order_reason(m);
                return Some(cmd);
            }
            let reason = if needs_hedge_only(gap, allowed_gap) {
                "rebalancing_quote_ineligible"
            } else {
                "hedge_only_quote_ineligible"
            };
            log_no_order_reason(ticker, m, reason, t_rem, Some(&signal), gap, allowed_gap);
            return None;
        }
        log_no_order_reason(
            ticker,
            m,
            "regime_blocks_new_imbalance",
            t_rem,
            Some(&signal),
            gap,
            allowed_gap,
        );
        return None;
    }

    match maybe_open_pair_quote(cfg, ticker, m, now, &signal) {
        PairOpenResult::Command(cmd) => {
            clear_no_order_reason(m);
            return Some(cmd);
        }
        PairOpenResult::Working => {
            clear_no_order_reason(m);
            return None;
        }
        PairOpenResult::Unavailable => {}
    }

    if let Some(cmd) = cancel_all_if_any(cfg, ticker, m, now) {
        clear_no_order_reason(m);
        return Some(cmd);
    }
    log_no_order_reason(
        ticker,
        m,
        "pair_open_quote_unavailable",
        t_rem,
        Some(&signal),
        gap,
        allowed_gap,
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_expensive_hedge_that_does_not_repair_floor_enough() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.pos.apply_fill(Side::Yes, 45, 1);
        m.pos.apply_fill(Side::Yes, 45, 1);
        m.pos.apply_fill(Side::Yes, 44, 1);
        m.pos.apply_fill(Side::Yes, 43, 1);
        m.pos.apply_fill(Side::No, 51, 1);

        assert!(!admission_ok(&cfg, &m, Side::No, 99, 1));
    }

    #[test]
    fn allows_balancing_fill_when_locked_floor_turns_positive() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.pos.apply_fill(Side::No, 51, 1);

        assert!(admission_ok(&cfg, &m, Side::Yes, 47, 1));
    }

    #[test]
    fn allows_loss_reducing_hedge_even_when_pair_cost_worsens() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.pos.apply_fill(Side::Yes, 38, 1);
        m.pos.apply_fill(Side::Yes, 38, 1);
        m.pos.apply_fill(Side::No, 58, 1);
        m.pos.apply_fill(Side::No, 58, 1);
        m.pos.apply_fill(Side::No, 58, 1);

        assert_eq!(m.pos.locked_floor_cc(), -5_000);
        assert_eq!(m.pos.pair_cost_cc(), Some(9_600));
        assert!(admission_ok(&cfg, &m, Side::Yes, 80, 1));
    }

    #[test]
    fn allows_opening_when_extreme_signal_is_still_close_to_kalshi_mid() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.book.yes_bids[70] = 1;
        m.book.no_bids[26] = 1;

        let signal = CoinbaseSignal {
            price: 0.0,
            microprice: 0.0,
            ema_fast: 0.0,
            ema_slow: 0.0,
            vol_ema_usd: 0.0,
            fair_yes: 0.80,
            fair_yes_cents: 80,
            trend_z: 0.0,
            distance_usd: 0.0,
            sigma_usd: 1.0,
            age_ms: 0,
            regime: SignalRegime::ExtremeUp,
            realized_final_avg: None,
            required_remaining_avg: None,
        };

        assert_eq!(kalshi_mid_yes_cents(&m), Some(72.0));
        assert_eq!(anchored_fair_yes_cents(&m, &signal), 75);
        assert!(can_open_pairs(
            &cfg,
            &m,
            &signal,
            cfg.no_new_imbalance_s + 1
        ));
    }

    #[test]
    fn pair_open_prices_prefer_live_top_pair_when_it_improves_the_book() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.pos.apply_fill(Side::Yes, 51, 1);
        m.pos.apply_fill(Side::No, 52, 1);
        m.book.yes_bids[46] = 1;
        m.book.no_bids[51] = 1;

        let signal = CoinbaseSignal {
            price: 0.0,
            microprice: 0.0,
            ema_fast: 0.0,
            ema_slow: 0.0,
            vol_ema_usd: 0.0,
            fair_yes: 0.55,
            fair_yes_cents: 55,
            trend_z: 0.0,
            distance_usd: 0.0,
            sigma_usd: 1.0,
            age_ms: 0,
            regime: SignalRegime::TwoSided,
            realized_final_avg: None,
            required_remaining_avg: None,
        };

        assert_eq!(m.pos.pair_cost_cc(), Some(10_300));
        assert_eq!(m.pos.locked_floor_cc(), -300);
        assert_eq!(pair_open_prices(&cfg, &m, &signal), Some((47, 52)));
    }

    #[test]
    fn balanced_bad_ignores_small_pair_cost_overage_with_hysteresis() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.pos.apply_fill(Side::Yes, 49, 1);
        m.pos.apply_fill(Side::Yes, 49, 1);
        m.pos.apply_fill(Side::Yes, 50, 1);
        m.pos.apply_fill(Side::No, 50, 1);
        m.pos.apply_fill(Side::No, 50, 1);
        m.pos.apply_fill(Side::No, 50, 1);

        assert_eq!(m.pos.pair_cost_cc(), Some(9_933));
        assert_eq!(m.pos.locked_floor_cc(), 200);
        assert!(!is_balanced_but_bad(&cfg, &m));
    }

    #[test]
    fn cancel_side_force_respects_min_resting_life() {
        let cfg = Config::default();
        let mut m = Market::new();
        let now = Instant::now();

        m.resting_yes = Some(RestingHint {
            price_cents: 47,
            created_at: now,
            cancel_requested_at: None,
            client_order_id: uuid::Uuid::new_v4(),
            order_id: Some("yes-1".to_string()),
            queue_ahead: 0,
        });

        assert!(cancel_side_force(&cfg, "TEST", &mut m, now, Side::Yes).is_none());
    }

    #[test]
    fn pair_open_quote_treats_matching_resting_pair_as_working() {
        let cfg = Config::default();
        let mut m = Market::new();
        let now = Instant::now();

        m.book.yes_bids[46] = 1;
        m.book.no_bids[51] = 1;
        m.resting_yes = Some(RestingHint {
            price_cents: 47,
            created_at: now,
            cancel_requested_at: None,
            client_order_id: uuid::Uuid::new_v4(),
            order_id: Some("yes-1".to_string()),
            queue_ahead: 0,
        });
        m.resting_no = Some(RestingHint {
            price_cents: 52,
            created_at: now,
            cancel_requested_at: None,
            client_order_id: uuid::Uuid::new_v4(),
            order_id: Some("no-1".to_string()),
            queue_ahead: 0,
        });

        let signal = CoinbaseSignal {
            price: 0.0,
            microprice: 0.0,
            ema_fast: 0.0,
            ema_slow: 0.0,
            vol_ema_usd: 0.0,
            fair_yes: 0.55,
            fair_yes_cents: 55,
            trend_z: 0.0,
            distance_usd: 0.0,
            sigma_usd: 1.0,
            age_ms: 0,
            regime: SignalRegime::TwoSided,
            realized_final_avg: None,
            required_remaining_avg: None,
        };

        assert!(matches!(
            maybe_open_pair_quote(&cfg, "TEST", &mut m, now, &signal),
            PairOpenResult::Working
        ));
    }

    #[test]
    fn repair_quote_must_improve_the_balanced_book() {
        let cfg = Config::default();
        let mut m = Market::new();

        m.pos.apply_fill(Side::Yes, 45, 1);
        m.pos.apply_fill(Side::Yes, 45, 1);
        m.pos.apply_fill(Side::Yes, 45, 1);
        m.pos.apply_fill(Side::Yes, 44, 1);
        m.pos.apply_fill(Side::No, 57, 1);
        m.pos.apply_fill(Side::No, 57, 1);
        m.pos.apply_fill(Side::No, 57, 1);
        m.pos.apply_fill(Side::No, 57, 1);

        m.book.yes_bids[40] = 1;
        m.book.no_bids[58] = 1;

        assert!(repair_quote_improves_book(&cfg, &m, Side::Yes, 32));
        assert!(!repair_quote_improves_book(&cfg, &m, Side::No, 65));
    }
}
