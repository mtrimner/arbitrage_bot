use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Live,
    Paper,
}

impl ExecMode {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "paper" => ExecMode::Paper,
            _ => ExecMode::Live,
        }
    }

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
    pub paper_reject_postonly_cross: bool,
    pub tick_ms: u64,

    pub series_tickers: Vec<String>,
    pub market_refresh_ms: u64,

    pub window_s: i64,
    pub accumulate_s: i64,
    pub balance_s: i64,

    pub aggressive_tick: u8,
    pub maker_improve_tick: u8,
    pub maker_improve_tick_balance: u8,
    pub max_buy_price_cents: u8,

    pub safe_pair_cc: i64,
    pub target_pair_cc: i64,
    pub bootstrap_pair_cc: i64,
    pub balance_pair_cc: i64,
    pub final_balance_pair_cc: i64,

    pub bootstrap_max_one_side_qty: i64,
    pub bootstrap_rescue_min_improve_cc: i64,

    pub early_imbalance_cap: f64,
    pub late_imbalance_cap: f64,
    pub imbalance_min_total: i64,
    pub imbalance_cap_small_total: f64,

    pub max_unhedged_qty_early: i64,
    pub max_unhedged_qty_late: i64,
    pub freeze_if_balanced_s: i64,

    pub max_order_qty: u64,
    pub catchup_aggressiveness: f64,
    pub catchup_balance_boost: f64,

    pub cancel_stale_ms: u64,
    pub min_resting_life_ms: u64,
    pub cancel_retry_ms: u64,
    pub cancel_drift_cents: u8,
    pub maker_max_edge_cents: u8,
    pub maker_max_edge_cents_balance: u8,
    pub maker_qty_price_tol_cents: u8,
    pub maker_qty_price_tol_cents_balance: u8,
    pub cancel_drift_cents_hedge: u8,
    pub hedge_force_ask_minus_one_gap: i64,

    pub taker_cooldown_ms: u64,
    pub min_taker_improve_cc: i64,
    pub maker_first_ms: u64,
    pub taker_desperate_s: i64,
    pub taker_big_improve_cc: i64,
    pub taker_force_gap: i64,

    pub short_side_min_order_qty: u64,

    pub coinbase_ws_enabled: bool,
    pub coinbase_product_id: String,
    pub coinbase_log_delta_usd: f64,
    pub coinbase_leadlag_enabled: bool,
    pub coinbase_leadlag_min_move_usd: f64,
    pub coinbase_leadlag_file: String,
    pub coinbase_leadlag_max_wait_ms: u64,
    pub coinbase_leadlag_min_kalshi_move_cents: u8,
    pub coinbase_leadlag_window_ms: u64,

    pub coinbase_stale_ms: u64,
    pub coinbase_history_ms: u64,
    pub coinbase_ema_fast_ms: u64,
    pub coinbase_ema_slow_ms: u64,
    pub coinbase_vol_ema_ms: u64,
    pub final_avg_window_s: i64,
    pub signal_late_threshold_s: i64,
    pub signal_two_sided_low: f64,
    pub signal_two_sided_high: f64,
    pub signal_extreme_low: f64,
    pub signal_extreme_high: f64,
    pub signal_pinned_low: f64,
    pub signal_pinned_high: f64,
    pub signal_two_sided_low_late: f64,
    pub signal_two_sided_high_late: f64,
    pub signal_extreme_low_late: f64,
    pub signal_extreme_high_late: f64,
    pub signal_pinned_low_late: f64,
    pub signal_pinned_high_late: f64,
    pub fair_sigma_floor_usd: f64,
    pub fair_vol_sqrt_scale: f64,
    pub fair_logistic_k: f64,
    pub fair_trend_weight: f64,
    pub cancel_trend_z: f64,

    pub quote_base_halfspread_cents: u8,
    pub quote_vol_per_extra_cent_usd: f64,
    pub quote_max_vol_extra_cents: u8,
    pub hedge_quote_boost_cents: u8,
    pub inventory_skew_per_contract_cents: u8,
    pub inventory_skew_max_cents: u8,
    pub market_entry_pair_cost_cc: i64,
    pub locked_floor_buffer_cc: i64,
    pub catchup_plausibility_buffer_cents: u8,
    pub no_new_imbalance_s: i64,

    pub results_file: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exec_mode: ExecMode::Live,
            paper_reject_postonly_cross: true,
            tick_ms: 250,
            window_s: 900,
            accumulate_s: 120,
            balance_s: 120,
            series_tickers: vec!["KXBTC15M".to_string()],
            market_refresh_ms: 5000,

            aggressive_tick: 1,
            maker_improve_tick: 1,
            maker_improve_tick_balance: 1,
            max_buy_price_cents: 99,

            safe_pair_cc: 9950,
            target_pair_cc: 9900,
            bootstrap_pair_cc: 9950,
            balance_pair_cc: 9950,
            final_balance_pair_cc: 10050,
            bootstrap_max_one_side_qty: 2,
            bootstrap_rescue_min_improve_cc: 500,

            early_imbalance_cap: 0.20,
            late_imbalance_cap: 0.10,
            imbalance_min_total: 20,
            imbalance_cap_small_total: 0.50,
            max_unhedged_qty_early: 2,
            max_unhedged_qty_late: 1,
            freeze_if_balanced_s: 45,

            max_order_qty: 10,
            catchup_aggressiveness: 0.45,
            catchup_balance_boost: 1.5,

            cancel_stale_ms: 30_000,
            min_resting_life_ms: 1_000,
            cancel_retry_ms: 600,
            cancel_drift_cents: 2,
            maker_max_edge_cents: 8,
            maker_max_edge_cents_balance: 12,
            maker_qty_price_tol_cents: 1,
            maker_qty_price_tol_cents_balance: 1,
            cancel_drift_cents_hedge: 1,
            hedge_force_ask_minus_one_gap: 1,

            taker_cooldown_ms: 1000,
            min_taker_improve_cc: 25,
            maker_first_ms: 1500,
            taker_desperate_s: 20,
            taker_big_improve_cc: 150,
            taker_force_gap: 2,
            short_side_min_order_qty: 1,

            coinbase_ws_enabled: true,
            coinbase_product_id: "BTC-USD".to_string(),
            coinbase_log_delta_usd: 10.0,
            coinbase_leadlag_enabled: true,
            coinbase_leadlag_min_move_usd: 10.0,
            coinbase_leadlag_file: "coinbase_kalshi_leadlag.csv".to_string(),
            coinbase_leadlag_max_wait_ms: 5000,
            coinbase_leadlag_min_kalshi_move_cents: 1,
            coinbase_leadlag_window_ms: 1000,

            coinbase_stale_ms: 1500,
            coinbase_history_ms: 240_000,
            coinbase_ema_fast_ms: 1500,
            coinbase_ema_slow_ms: 6000,
            coinbase_vol_ema_ms: 6000,
            final_avg_window_s: 60,
            signal_late_threshold_s: 90,
            signal_two_sided_low: 0.35,
            signal_two_sided_high: 0.65,
            signal_extreme_low: 0.20,
            signal_extreme_high: 0.80,
            signal_pinned_low: 0.10,
            signal_pinned_high: 0.90,
            signal_two_sided_low_late: 0.40,
            signal_two_sided_high_late: 0.60,
            signal_extreme_low_late: 0.25,
            signal_extreme_high_late: 0.75,
            signal_pinned_low_late: 0.15,
            signal_pinned_high_late: 0.85,
            fair_sigma_floor_usd: 35.0,
            fair_vol_sqrt_scale: 1.25,
            fair_logistic_k: 1.8,
            fair_trend_weight: 0.07,
            cancel_trend_z: 0.30,

            quote_base_halfspread_cents: 2,
            quote_vol_per_extra_cent_usd: 20.0,
            quote_max_vol_extra_cents: 3,
            hedge_quote_boost_cents: 1,
            inventory_skew_per_contract_cents: 1,
            inventory_skew_max_cents: 3,
            market_entry_pair_cost_cc: 9800,
            locked_floor_buffer_cc: 100,
            catchup_plausibility_buffer_cents: 1,
            no_new_imbalance_s: 30,

            results_file: "results.csv".to_string(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        cfg.exec_mode = ExecMode::from_env();

        if let Ok(v) = env::var("RESULTS_FILE") {
            cfg.results_file = v;
        }

        if let Ok(v) = env::var("COINBASE_WS") {
            let s = v.trim().to_ascii_lowercase();
            cfg.coinbase_ws_enabled = matches!(s.as_str(), "1" | "true" | "yes" | "y" | "on");
        }
        if let Ok(v) = env::var("COINBASE_PRODUCT_ID") {
            let s = v.trim();
            if !s.is_empty() {
                cfg.coinbase_product_id = s.to_string();
            }
        }
        if let Ok(v) = env::var("COINBASE_LOG_DELTA_USD") {
            if let Ok(x) = v.trim().parse::<f64>() {
                if x.is_finite() && x > 0.0 {
                    cfg.coinbase_log_delta_usd = x;
                }
            }
        }
        if let Ok(v) = env::var("COINBASE_STALE_MS") {
            if let Ok(x) = v.trim().parse::<u64>() {
                cfg.coinbase_stale_ms = x.max(250);
            }
        }
        if let Ok(v) = env::var("QUOTE_BASE_HALFSPREAD_CENTS") {
            if let Ok(x) = v.trim().parse::<u8>() {
                cfg.quote_base_halfspread_cents = x.max(1);
            }
        }
        if let Ok(v) = env::var("MARKET_ENTRY_PAIR_CC") {
            if let Ok(x) = v.trim().parse::<i64>() {
                cfg.market_entry_pair_cost_cc = x.max(9000);
            }
        }
        if let Ok(v) = env::var("LOCKED_FLOOR_BUFFER_CC") {
            if let Ok(x) = v.trim().parse::<i64>() {
                cfg.locked_floor_buffer_cc = x.max(0);
            }
        }

        cfg
    }
}
