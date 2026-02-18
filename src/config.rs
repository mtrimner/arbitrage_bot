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

    // Which series you want to trade simultaneously.
    // One active market per series at a time.
    pub series_tickers: Vec<String>,

    // How often the MarketManager checks for expirations + rotates.
    pub market_refresh_ms: u64,

    // Window definition (15 minutes => 900 seconds)
    pub window_s: i64,
    pub accumulate_s: i64, // “early” phase length
    pub balance_s: i64,    // “late” phase length (force balancing)

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exec_mode: ExecMode::Live,
            paper_reject_postonly_cross: true,

            tick_ms: 25,

            window_s: 900,
            accumulate_s: 150,
            balance_s: 500,

            series_tickers: vec!["KXBTC15M".to_string()],
            market_refresh_ms: 5000,

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

            early_imbalance_cap: 0.20,
            late_imbalance_cap: 0.05,

            max_order_qty: 25,
            catchup_aggressiveness: 0.35,
            catchup_balance_boost: 1.0,

            cancel_stale_ms: 15000,
            min_resting_life_ms: 1000,
            cancel_retry_ms: 800,
            cancel_drift_cents: 3,
            maker_max_edge_cents: 8,

            taker_cooldown_ms: 500,
            min_taker_improve_cc: 20, // 0.2 cents improvement in pair-cost

            maker_first_ms: 1500,      // 1.5s
            taker_desperate_s: 30,     // last 30s
            taker_big_improve_cc: 100, // 1.00 cent improvement in pair-cost
        }
    }
}
