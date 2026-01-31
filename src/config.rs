/// Strategy + microstructure tuning parameters.
///
/// IMPORTANT: you will absolutely need to tune these live.
/// Defaults here are “reasonable starter values” for a 15-minute market.
#[derive(Debug, Clone)]
pub struct Config {
    // How often the engine runs.
    // Even if your WS updates are fast, 20–50ms is usually plenty.
    pub tick_ms: u64,

    // Which series you want to trade simultaneously.
    // One active market per series at a time.
    pub series_tickers: Vec<String>,

    // How often the MarketManager checks for expirations + rotates.
    pub market_refresh_ms: u64,

    // Window definition (15 minutes => 900 seconds)
    pub window_s: i64,
    pub accumulate_s: i64, // “early” phase length
    pub balance_s: i64,    // “late” phase length (force balancing)

    // Momentum usage: how many “extra” buys you allow in direction of flow
    // when you are otherwise balanced.
    pub momentum_base_extra: i64,
    pub momentum_min_extra: i64,
    pub momentum_score_threshold: f64,

    // Maker/taker price constraints.
    pub aggressive_tick: u8,     // used for “slightly more aggressive” logic
    pub maker_improve_tick: u8,  // how much we improve best bid when quoting (often 0 or 1)
    pub max_buy_price_cents: u8, // never pay above this

    // Pair cost goal (in cent-cents, see CC_PER_CENT in types.rs)
    pub safe_pair_cc: i64,   // “never exceed” (looser) cap
    pub target_pair_cc: i64, // “goal” cap

    // Inventory imbalance limits (ratio of |yes-no| / (yes+no)).
    pub early_imbalance_cap: f64,
    pub late_imbalance_cap: f64,

    // Resting order management
    pub cancel_stale_ms: u64,        // cancel resting orders older than this
    pub min_resting_life_ms: u64,    // DO NOT churn/cancel before this age
    pub cancel_retry_ms: u64,        // if we sent cancel already, wait this long to retry
    pub cancel_drift_cents: u8,      // if desired quote moves away from current resting price by >= this, consider requote
    pub maker_max_edge_cents: u8,    // don’t quote more than this below “top maker price” (avoids super-low bids that never fill)

    // Opportunistic taker behavior
    pub taker_cooldown_ms: u64, // don’t fire takers on same side more often than this
    pub min_taker_improve_cc: i64, // require at least this much pair-cost improvement (cent-cents) for opportunistic taker (unless balancing)

    // Feature smoothing / signal model
    //
    // We use EMA smoothing: value(t) = value(t-Δt) + α(Δt)*(x - value(t-Δt))
    // where α(Δt) = 1 - exp(-Δt / τ)
    //
    // τ (tau) is the “time constant”: bigger τ => smoother / slower response.
    pub tau_book_ms: u64,
    pub tau_trade_ms: u64,
    pub tau_delta_ms: u64,
    pub tau_score_ms: u64,

    // How we down-weight trade/delta features when activity is low:
    // count events in last rate_window_ms; full weight after *full_weight_count* events.
    pub rate_window_ms: u64,
    pub trade_full_weight_count: usize,
    pub delta_full_weight_count: usize,

    // How much absolute delta (sum of |delta| in the rate window)
    // should correspond to "full confidence" for delta weighting.
    pub delta_full_weight_abs: u32,

    // Base weights for combining features into one “pressure score”
    // (positive => YES pressure; negative => NO pressure)
    pub w_book: f64,
    pub w_trade: f64,
    pub w_delta: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tick_ms: 25,

            window_s: 900,
            accumulate_s: 300,
            balance_s: 300,

            series_tickers: vec!["KXBTC15M".to_string()],
            market_refresh_ms: 5000,

            momentum_base_extra: 3,
            momentum_min_extra: 0,
            momentum_score_threshold: 0.12,

            aggressive_tick: 1,
            maker_improve_tick: 0,
            max_buy_price_cents: 90,

            safe_pair_cc: 9850,
            target_pair_cc: 9825,

            early_imbalance_cap: 0.20,
            late_imbalance_cap: 0.05,

            cancel_stale_ms: 5000,
            min_resting_life_ms: 250,
            cancel_retry_ms: 800,
            cancel_drift_cents: 2,
            maker_max_edge_cents: 8,

            taker_cooldown_ms: 400,
            min_taker_improve_cc: 5, // 0.05 cents improvement in pair-cost

            tau_book_ms: 700,
            tau_trade_ms: 900,
            tau_delta_ms: 800,
            tau_score_ms: 600,

            rate_window_ms: 10_000,
            trade_full_weight_count: 15, // full confidence once ~25 trades in last 10s
            delta_full_weight_count: 250, // deltas tend to be more frequent than trades
            delta_full_weight_abs: 80000,

            w_book: 0.35,
            w_trade: 0.45,
            w_delta: 0.20,
        }
    }
}
