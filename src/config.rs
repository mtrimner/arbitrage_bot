use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Live,
    Paper,
}

impl ExecMode{
    /// Parse an execution mode from a string
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "paper" => ExecMode::Paper,
            _ => ExecMode::Live,
        }
    }

    /// Read `EXEC_MODE`` from the environment.
    pub fn from_env() -> Self {
        let raw = env::var("EXEC_MODE").unwrap_or_else(|_| "paper".to_string());
        Self::parse(&raw)
    }

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

    // If total position is small, ratio is too “jumpy” (e.g., 1 vs 2 = 0.333).
    // Until total >= this, we relax the imbalance cap to at least `imbalance_cap_small_total`.
    pub imbalance_min_total: i64,
    pub imbalance_cap_small_total: f64,

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
    pub maker_qty_price_tol_cents: u8,          // price-vs-qty tolerance when choosing (price,qty) under cap (normal)
    pub maker_qty_price_tol_cents_balance: u8,  // same tolerance in Balance mode

    // -------- Inventory-skewed dual quoting knobs --------
    // When imbalance_ratio >= this, we skew quoting:
    // - hedge side becomes "more competitive" (can force top to ask-1 in maker quote)
    // - strong side becomes "weak" and only quotes if it materially improves pair cost
    pub skew_imbalance_start: f64,

    // Hedge side reprices faster (e.g. 1 cent), strong side uses cancel_drift_cents.
    pub cancel_drift_cents_hedge: u8,

    // If desired_side == hedge and imbalance_ratio >= this, push quote up to ask-1 (still post-only).
    pub hedge_force_ask_minus_one_imbalance: f64,

    // Strong-side passive quote constraints
    pub dual_strong_min_improve_cc: i64, // require at least this much pair-cost improvement (cent-cents)
    pub dual_strong_backoff_cents: u8,   // how many cents below top-maker to start searching for a weak bid
    pub dual_strong_qty: u64,            // qty for strong-side passive order (usually 1)

    // Don’t enable skew-dual quoting until we have enough inventory (prevents early “forced hedge” stalls).
    pub skew_min_total: i64,

    // Opportunistic taker behavior
    pub taker_cooldown_ms: u64, // don’t fire takers on same side more often than this
    pub min_taker_improve_cc: i64, // require at least this much pair-cost improvement (cent-cents) for opportunistic taker (unless balancing)

    pub maker_first_ms: u64,      // wait this long for a resting maker to work
    pub taker_desperate_s: i64,   // only force IOC in last N seconds of Balance
    pub taker_big_improve_cc: i64, // allow IOC early only for huge improvements

    // --- Short-side sizing ---
    /// When we are buying the **short** side (gap > 0), try at least this many contracts.
    /// Set to 1 to disable.
    ///
    /// NOTE: this is only the *desired* qty. The maker price/qty chooser may still fall
    /// back to a smaller qty (down to 1) if larger sizes can't be priced under cap.
    pub short_side_min_order_qty: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exec_mode: ExecMode::Live,
            paper_reject_postonly_cross: true,

            tick_ms: 250,

            window_s: 900,
            accumulate_s: 150,
            balance_s: 240,

            series_tickers: vec!["KXBTC15M".to_string()],
            market_refresh_ms: 5000,

            aggressive_tick: 1,
            maker_improve_tick: 1,
            maker_improve_tick_balance: 99,
            max_buy_price_cents: 99,

            safe_pair_cc: 9875,
            target_pair_cc: 9825,

            bootstrap_pair_cc: 10100, // $1.01
            balance_pair_cc: 9925, // $0.99.25
 
            bootstrap_max_one_side_qty: 5,
            bootstrap_rescue_min_improve_cc: 500,

            early_imbalance_cap: 0.20,
            late_imbalance_cap: 0.10,

            imbalance_min_total: 20,
            imbalance_cap_small_total: 0.50,

            max_order_qty: 25,
            catchup_aggressiveness: 0.45,
            catchup_balance_boost: 1.5,

            cancel_stale_ms: 120000,
            min_resting_life_ms: 1000,
            cancel_retry_ms: 800,
            cancel_drift_cents: 3,
            maker_max_edge_cents: 15,
            // If qty>1 forces you to quote much lower, stick to smaller qty near top
            maker_qty_price_tol_cents: 2,
            maker_qty_price_tol_cents_balance: 1,

            // Inventory-skew defaults (tune these!)
            skew_imbalance_start: 0.05,
            cancel_drift_cents_hedge: 1,
            hedge_force_ask_minus_one_imbalance: 0.10,
            dual_strong_min_improve_cc: 20, // 0.20 cents
            dual_strong_backoff_cents: 3,
            dual_strong_qty: 1,
            skew_min_total: 10,

            taker_cooldown_ms: 1000,
            min_taker_improve_cc: 20, // 0.2 cents improvement in pair-cost

            maker_first_ms: 1500,      // 1.5s
            taker_desperate_s: 120,     // last 120s
            taker_big_improve_cc: 100, // 1.00 cent improvement in pair-cost

            short_side_min_order_qty: 3
        }
    }
}

impl Config {
    /// Load config from environment variables (overrides defaults).
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        cfg.exec_mode = ExecMode::from_env();
        cfg
    }
}
