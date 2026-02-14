// src/report.rs
use tracing::info;

use crate::state::position::Position;
use crate::types::CC_PER_CENT;

fn cc_to_cents(cc: i64) -> f64 {
    cc as f64 / CC_PER_CENT as f64
}
fn cc_to_dollars(cc: i64) -> f64 {
    cc as f64 / (CC_PER_CENT as f64 * 100.0)
}

pub fn log_position(ticker: &str, pos: &Position) {
    let yes_avg_cents = pos.avg_yes_cc().map(cc_to_cents);
    let no_avg_cents  = pos.avg_no_cc().map(cc_to_cents);

    // “total avg price” in your world is really “pair cost” (avg_yes + avg_no)
    let pair_cost_cents  = pos.pair_cost_cc().map(cc_to_cents);
    let pair_cost_dollars = pos.pair_cost_cc().map(cc_to_dollars);

    info!(
        ticker = %ticker,
        yes_qty = pos.yes_qty,
        no_qty = pos.no_qty,
        yes_avg_cents = ?yes_avg_cents,
        no_avg_cents = ?no_avg_cents,
        pair_cost_cents = ?pair_cost_cents,
        pair_cost_dollars = ?pair_cost_dollars,
        "position snapshot"
    );
}
