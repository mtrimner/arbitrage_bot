#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Live,
    Paper,
}

impl ExecMode {
    pub fn is_paper(self) -> bool {
        matches!(self, ExecMode::Paper)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub exec_mode: ExecMode,
    // (optional) realism knobs:
    pub paper_reject_postonly_cross: bool,
    // How often the engine runs.
    // Even if your WS updates are fast, 20–50ms is usually plenty.
    pub tick_ms: u64,

    pub side_exit_mult: f64, // e.g. 0.6 (exit < side)
    pub min_conf_for_flow: f64,
    pub min_conf_for_momentum: f64,

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
    pub maker_improve_tick_balance: u8, // in balance mode, basically forces maker to ask-1
    pub max_buy_price_cents: u8, // never pay above this

    // Pair cost goal (in cent-cents, see CC_PER_CENT in types.rs)
    pub safe_pair_cc: i64,   // “never exceed” (looser) cap
    pub target_pair_cc: i64, // “goal” cap

    // Bootstrapping / balancing caps:
    // - bootstrap_pair_cc: allowed avg_yes+avg_no while trying to establish the FIRST pair
    // - balance_pair_cc: allowed avg_yes+avg_no when forcing balance near the end
    pub bootstrap_pair_cc: i64,
    pub balance_pair_cc: i64,

    // One-sided “rescue” behavior:
    // - max unhedged qty allowed while waiting for the other side
    // - minimum improvement in avg (cent-cents) required to add more to the same side
    pub bootstrap_max_one_side_qty: i64,
    pub bootstrap_rescue_min_improve_cc: i64,

    // Inventory imbalance limits (ratio of |yes-no| / (yes+no)).
    pub early_imbalance_cap: f64,
    pub late_imbalance_cap: f64,

    // Dynamic sizing (catch-up)
    pub max_order_qty: u64,            // hard safety cap
    pub catchup_aggressiveness: f64,   // 0.0..1.0 how fast to catch up
    pub catchup_balance_boost: f64,    // multiplier in Balance mode


    // Resting order management
    pub cancel_stale_ms: u64,        // cancel resting orders older than this
    pub min_resting_life_ms: u64,    // DO NOT churn/cancel before this age
    pub cancel_retry_ms: u64,        // if we sent cancel already, wait this long to retry
    pub cancel_drift_cents: u8,      // if desired quote moves away from current resting price by >= this, consider requote
    pub maker_max_edge_cents: u8,    // don’t quote more than this below “top maker price” (avoids super-low bids that never fill)

    // Opportunistic taker behavior
    pub taker_cooldown_ms: u64, // don’t fire takers on same side more often than this
    pub min_taker_improve_cc: i64, // require at least this much pair-cost improvement (cent-cents) for opportunistic taker (unless balancing)

    pub maker_first_ms: u64,      // wait this long for a resting maker to work
    pub taker_desperate_s: i64,   // only force IOC in last N seconds of Balance
    pub taker_big_improve_cc: i64, // allow IOC early only for huge improvements

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
    // Trade magnitude in the rate window that corresponds to "full confidence"
    // for trade weighting (sum of trade counts in the window).
    pub trade_full_weight_abs: u32,

    // Confidence should not be high when the signal itself is tiny.
    // When |score_ema| >= this, strength factor ~= 1.0.
    pub score_full_conf_abs: f64,

    // Optional: normalize trade/delta magnitude by current top-of-book depth.
    // This makes the same activity "mean more" in thin books.
    pub enable_depth_norm: bool,
    pub depth_norm_levels: usize,     // how many top bid levels per side
    pub depth_full_weight_qty: f64,   // reference depth; smaller depth => boost
    pub depth_norm_min_mult: f64,     // clamp boost
    pub depth_norm_max_mult: f64,

    // Base weights for combining features into one “pressure score”
    // (positive => YES pressure; negative => NO pressure)
    pub w_book: f64,
    pub w_trade: f64,
    pub w_delta: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exec_mode: ExecMode::Live,
            paper_reject_postonly_cross: true,

            tick_ms: 25,

            side_exit_mult: 0.6,
            min_conf_for_flow: 0.35,
            min_conf_for_momentum: 0.5,

            window_s: 900,
            accumulate_s: 300,
            balance_s: 300,

            series_tickers: vec!["KXBTC15M".to_string()],
            market_refresh_ms: 5000,

            momentum_base_extra: 3,
            momentum_min_extra: 0,
            momentum_score_threshold: 0.12,

            aggressive_tick: 1,
            maker_improve_tick: 1,
            maker_improve_tick_balance: 99,
            max_buy_price_cents: 98,

            safe_pair_cc: 9850,
            target_pair_cc: 9825,

            bootstrap_pair_cc: 10100, // $1.01
            balance_pair_cc: 9900, // $0.99
 
            bootstrap_max_one_side_qty: 5,
            bootstrap_rescue_min_improve_cc: 500,

            early_imbalance_cap: 0.90,
            late_imbalance_cap: 0.10,

            max_order_qty: 25,
            catchup_aggressiveness: 0.35,
            catchup_balance_boost: 1.0,

            cancel_stale_ms: 5000,
            min_resting_life_ms: 250,
            cancel_retry_ms: 800,
            cancel_drift_cents: 1,
            maker_max_edge_cents: 8,

            taker_cooldown_ms: 500,
            min_taker_improve_cc: 20, // 0.2 cents improvement in pair-cost

            maker_first_ms: 1500,      // 1.5s
            taker_desperate_s: 30,     // last 30s
            taker_big_improve_cc: 100, // 1.00 cent improvement in pair-cost

            tau_book_ms: 300,
            tau_trade_ms: 3000,
            tau_delta_ms: 350,
            tau_score_ms: 400,

            rate_window_ms: 10_000,
            trade_full_weight_count: 10,
            delta_full_weight_count: 200, // deltas tend to be more frequent than trades
            delta_full_weight_abs: 6000,
            trade_full_weight_abs: 120,
            score_full_conf_abs: 0.35,

            enable_depth_norm: true,
            depth_norm_levels: 3,
            depth_full_weight_qty: 100.0,
            depth_norm_min_mult: 0.7,
            depth_norm_max_mult: 1.6,

            w_book: 0.35,
            w_trade: 0.45,
            w_delta: 0.20,
        }
    }
}
