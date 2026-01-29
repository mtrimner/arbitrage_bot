use std::time::Instant;

use crate::config::Config;
use crate::state::book::Book;
use crate::state::ticker::Market;

/// Weighted depth helper: sums the top N bid levels with decaying weights.
/// This is a “snapshot liquidity pressure” measure.
fn weighted_top_n_qty(arr: &[i64; 101], n: usize) -> f64 {
    // Simple decay weights: 1.0, 0.8, 0.6, ...
    let mut acc = 0.0;
    let mut found = 0usize;

    for p in (0..=100).rev() {
        let q = arr[p];
        if q <= 0 {
            continue;
        }
        let w = (1.0 - 0.2 * (found as f64)).max(0.2);
        acc += (q as f64) * w;
        found += 1;
        if found >= n {
            break;
        }
    }

    acc
}

/// Raw book imbalance in [-1, +1].
/// + => more YES bid depth near top
/// - => more NO bid depth near top
pub fn raw_book_imbalance(cfg: &Config, book: &Book) -> f64 {
    // You can tune N; 5 is a decent default for thin books.
    let n = 5;
    let y = weighted_top_n_qty(&book.yes_bids, n);
    let nqty = weighted_top_n_qty(&book.no_bids, n);

    let denom = (y + nqty).max(1.0);
    ((y - nqty) / denom).clamp(-1.0, 1.0)
}

/// Combine EMA features into one score.
/// This is the “prediction” you asked about:
/// - book_imb_ema: where liquidity is leaning (resting interest)
/// - trade_flow_ema: what is actually executing (aggressive pressure)
/// - delta_flow_ema: are bids being added/canceled (queue building / pulling)
///
/// We also confidence-scale trade/delta weights if recent activity is low.
pub fn combined_score(cfg: &Config, m: &mut Market) -> (f64, f64) {
    let now = Instant::now();

    // Count recent activity for confidence.
    let trade_n = m.flow.trade_count_recent(cfg, now);
    let delta_n = m.flow.delta_count_recent(cfg, now);

    let trade_factor = (trade_n as f64 / cfg.trade_full_weight_count as f64).clamp(0.0, 1.0);
    let delta_factor = (delta_n as f64 / cfg.delta_full_weight_count as f64).clamp(0.0, 1.0);

    let w_book = cfg.w_book;
    let w_trade = cfg.w_trade * trade_factor;
    let w_delta = cfg.w_delta * delta_factor;

    let w_sum = (w_book + w_trade + w_delta).max(1e-9);

    let raw = (w_book * m.flow.book_imb_ema.value
        + w_trade * m.flow.trade_flow_ema.value
        + w_delta * m.flow.delta_flow_ema.value)
        / w_sum;

    // Smooth the final score too (prevents jittery side flips).
    m.flow.on_score(cfg, raw, now);

    // Confidence is “how much of our max weights are active right now”.
    let conf = (w_sum / (cfg.w_book + cfg.w_trade + cfg.w_delta)).clamp(0.0, 1.0);

    (m.flow.score_ema.value.clamp(-1.0, 1.0), conf)
}
