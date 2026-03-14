use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::types::Side;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoinbaseBookSide {
    Bid,
    Ask,
}

#[derive(Debug, Clone)]
pub struct CoinbaseSample {
    pub ts_ms: u64,
    pub price: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalRegime {
    TwoSided,
    DriftUp,
    DriftDown,
    ExtremeUp,
    ExtremeDown,
    PinnedUp,
    PinnedDown,
}

#[derive(Debug, Clone)]
pub struct CoinbaseSignal {
    pub price: f64,
    pub microprice: f64,
    pub ema_fast: f64,
    pub ema_slow: f64,
    pub vol_ema_usd: f64,
    pub fair_yes: f64,
    pub fair_yes_cents: u8,
    pub trend_z: f64,
    pub distance_usd: f64,
    pub sigma_usd: f64,
    pub age_ms: u64,
    pub regime: SignalRegime,
    pub realized_final_avg: Option<f64>,
    pub required_remaining_avg: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CoinbaseSnapshot {
    pub product_id: String,
    pub last_trade_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_bid_qty: Option<f64>,
    pub best_ask_qty: Option<f64>,
    pub microprice: Option<f64>,
    pub ema_fast: Option<f64>,
    pub ema_slow: Option<f64>,
    pub vol_ema_usd: Option<f64>,
    pub last_update_ms: Option<u64>,
    pub last_heartbeat_ms: Option<u64>,
    pub heartbeat_counter: Option<u64>,
    pub samples: Vec<CoinbaseSample>,
}

impl Default for CoinbaseSnapshot {
    fn default() -> Self {
        Self {
            product_id: String::new(),
            last_trade_price: None,
            best_bid: None,
            best_ask: None,
            best_bid_qty: None,
            best_ask_qty: None,
            microprice: None,
            ema_fast: None,
            ema_slow: None,
            vol_ema_usd: None,
            last_update_ms: None,
            last_heartbeat_ms: None,
            heartbeat_counter: None,
            samples: Vec::new(),
        }
    }
}

impl CoinbaseSnapshot {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn time_weighted_average(&self, start_ms: u64, end_ms: u64) -> Option<f64> {
        if start_ms >= end_ms {
            return None;
        }
        let first = self.samples.first()?;

        let mut last_price = first.price;
        for sample in &self.samples {
            if sample.ts_ms <= start_ms {
                last_price = sample.price;
            } else {
                break;
            }
        }

        let mut last_ts = start_ms;
        let mut weighted = 0.0;

        for sample in &self.samples {
            if sample.ts_ms <= start_ms {
                continue;
            }
            if sample.ts_ms >= end_ms {
                break;
            }

            let dt = sample.ts_ms.saturating_sub(last_ts);
            if dt > 0 {
                weighted += last_price * dt as f64;
                last_ts = sample.ts_ms;
            }
            last_price = sample.price;
        }

        let tail_dt = end_ms.saturating_sub(last_ts);
        if tail_dt > 0 {
            weighted += last_price * tail_dt as f64;
        }

        let total = end_ms.saturating_sub(start_ms);
        if total == 0 {
            None
        } else {
            Some(weighted / total as f64)
        }
    }

    fn late_thresholds(cfg: &Config, t_rem: i64) -> (f64, f64, f64, f64, f64, f64) {
        if t_rem <= cfg.signal_late_threshold_s {
            (
                cfg.signal_two_sided_low_late,
                cfg.signal_two_sided_high_late,
                cfg.signal_extreme_low_late,
                cfg.signal_extreme_high_late,
                cfg.signal_pinned_low_late,
                cfg.signal_pinned_high_late,
            )
        } else {
            (
                cfg.signal_two_sided_low,
                cfg.signal_two_sided_high,
                cfg.signal_extreme_low,
                cfg.signal_extreme_high,
                cfg.signal_pinned_low,
                cfg.signal_pinned_high,
            )
        }
    }

    pub fn build_signal(
        &self,
        cfg: &Config,
        strike_price: f64,
        close_ts: Option<i64>,
        t_rem: i64,
    ) -> Option<CoinbaseSignal> {
        let last_update_ms = self.last_update_ms?;
        let age_ms = Self::now_ms().saturating_sub(last_update_ms);
        if age_ms > cfg.coinbase_stale_ms {
            return None;
        }

        let price = self.last_trade_price.or(self.microprice).or_else(|| {
            match (self.best_bid, self.best_ask) {
                (Some(b), Some(a)) => Some((a + b) * 0.5),
                _ => None,
            }
        })?;
        let microprice = self.microprice.unwrap_or(price);
        let ema_fast = self.ema_fast.unwrap_or(microprice);
        let ema_slow = self.ema_slow.unwrap_or(ema_fast);
        let vol_ema = self.vol_ema_usd.unwrap_or(cfg.fair_sigma_floor_usd);

        let mut distance_usd = ema_fast - strike_price;
        let mut realized_final_avg = None;
        let mut required_remaining_avg = None;

        if let Some(close_ts) = close_ts {
            let close_ms = (close_ts.max(0) as u64).saturating_mul(1000);
            let avg_window_ms = (cfg.final_avg_window_s.max(1) as u64).saturating_mul(1000);
            let final_start_ms = close_ms.saturating_sub(avg_window_ms);

            if last_update_ms >= final_start_ms && last_update_ms < close_ms {
                realized_final_avg = self.time_weighted_average(final_start_ms, last_update_ms);

                if let Some(realized) = realized_final_avg {
                    let elapsed_ms = last_update_ms.saturating_sub(final_start_ms);
                    let remaining_ms = close_ms.saturating_sub(last_update_ms);

                    if remaining_ms > 0 {
                        let total_secs = cfg.final_avg_window_s.max(1) as f64;
                        let elapsed_secs = (elapsed_ms as f64 / 1000.0).clamp(0.0, total_secs);
                        let remaining_secs = (remaining_ms as f64 / 1000.0).max(0.001);
                        let needed = ((total_secs * strike_price) - (elapsed_secs * realized))
                            / remaining_secs;
                        required_remaining_avg = Some(needed);
                        distance_usd = microprice - needed;
                    }
                }
            }
        }

        let sigma_usd = (cfg.fair_sigma_floor_usd
            + vol_ema * (t_rem.max(1) as f64).sqrt() * cfg.fair_vol_sqrt_scale)
            .max(cfg.fair_sigma_floor_usd);

        let z = if sigma_usd > 0.0 {
            distance_usd / sigma_usd
        } else {
            0.0
        };
        let logistic = 1.0 / (1.0 + (-cfg.fair_logistic_k * z).exp());
        let trend_z = ((ema_fast - ema_slow) / sigma_usd).clamp(-1.5, 1.5);
        let fair_yes = (logistic + cfg.fair_trend_weight * trend_z).clamp(0.001, 0.999);
        let fair_yes_cents = (fair_yes * 100.0).round().clamp(1.0, 99.0) as u8;

        let (two_low, two_high, extreme_low, extreme_high, pinned_low, pinned_high) =
            Self::late_thresholds(cfg, t_rem);

        let regime = if fair_yes >= pinned_high {
            SignalRegime::PinnedUp
        } else if fair_yes <= pinned_low {
            SignalRegime::PinnedDown
        } else if fair_yes >= extreme_high {
            SignalRegime::ExtremeUp
        } else if fair_yes <= extreme_low {
            SignalRegime::ExtremeDown
        } else if fair_yes >= two_high {
            SignalRegime::DriftUp
        } else if fair_yes <= two_low {
            SignalRegime::DriftDown
        } else {
            SignalRegime::TwoSided
        };

        Some(CoinbaseSignal {
            price,
            microprice,
            ema_fast,
            ema_slow,
            vol_ema_usd: vol_ema,
            fair_yes,
            fair_yes_cents,
            trend_z,
            distance_usd,
            sigma_usd,
            age_ms,
            regime,
            realized_final_avg,
            required_remaining_avg,
        })
    }

    pub fn cancel_vulnerable_side(&self, signal: &CoinbaseSignal, cfg: &Config) -> Option<Side> {
        if signal.trend_z >= cfg.cancel_trend_z {
            Some(Side::No)
        } else if signal.trend_z <= -cfg.cancel_trend_z {
            Some(Side::Yes)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoinbaseState {
    pub product_id: String,
    pub last_trade_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_bid_qty: Option<f64>,
    pub best_ask_qty: Option<f64>,
    pub microprice: Option<f64>,
    pub ema_fast: Option<f64>,
    pub ema_slow: Option<f64>,
    pub vol_ema_usd: Option<f64>,
    pub last_update_ms: Option<u64>,
    pub last_heartbeat_ms: Option<u64>,
    pub heartbeat_counter: Option<u64>,
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
    samples: VecDeque<CoinbaseSample>,
}

impl CoinbaseState {
    pub fn new(product_id: String) -> Self {
        Self {
            product_id,
            last_trade_price: None,
            best_bid: None,
            best_ask: None,
            best_bid_qty: None,
            best_ask_qty: None,
            microprice: None,
            ema_fast: None,
            ema_slow: None,
            vol_ema_usd: None,
            last_update_ms: None,
            last_heartbeat_ms: None,
            heartbeat_counter: None,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            samples: VecDeque::new(),
        }
    }

    fn price_to_key(price: f64) -> Option<i64> {
        if !price.is_finite() || price <= 0.0 {
            return None;
        }
        Some((price * 100.0).round() as i64)
    }

    fn key_to_price(key: i64) -> f64 {
        key as f64 / 100.0
    }

    fn update_ema(prev: Option<f64>, next: f64, dt_ms: u64, span_ms: u64) -> f64 {
        match prev {
            None => next,
            Some(_cur) if span_ms == 0 => next,
            Some(cur) => {
                let alpha = 1.0 - (-((dt_ms.max(1) as f64) / span_ms as f64)).exp();
                cur + alpha * (next - cur)
            }
        }
    }

    fn choose_top_of_book(&mut self) {
        if let Some((&bid_key, &qty)) = self.bids.last_key_value() {
            self.best_bid = Some(Self::key_to_price(bid_key));
            self.best_bid_qty = Some(qty);
        } else {
            self.best_bid = None;
            self.best_bid_qty = None;
        }
        if let Some((&ask_key, &qty)) = self.asks.first_key_value() {
            self.best_ask = Some(Self::key_to_price(ask_key));
            self.best_ask_qty = Some(qty);
        } else {
            self.best_ask = None;
            self.best_ask_qty = None;
        }
    }

    fn refresh_price_state(&mut self, cfg: &Config, ts_ms: u64) -> bool {
        let prev_price = self.microprice.or(self.last_trade_price);
        let price = match (self.best_bid, self.best_ask, self.last_trade_price) {
            (Some(b), Some(a), _) if a >= b => {
                let bid_qty = self.best_bid_qty.unwrap_or(0.0);
                let ask_qty = self.best_ask_qty.unwrap_or(0.0);
                let mp = if bid_qty > 0.0 && ask_qty > 0.0 {
                    (a * bid_qty + b * ask_qty) / (bid_qty + ask_qty)
                } else {
                    (a + b) * 0.5
                };
                self.microprice = Some(mp);
                mp
            }
            _ => {
                let p = self.last_trade_price.or(self.microprice);
                self.microprice = p;
                let Some(p) = p else {
                    return false;
                };
                p
            }
        };

        let dt_ms = self
            .last_update_ms
            .map(|prev| ts_ms.saturating_sub(prev))
            .unwrap_or(cfg.tick_ms.max(1));

        self.ema_fast = Some(Self::update_ema(
            self.ema_fast,
            price,
            dt_ms,
            cfg.coinbase_ema_fast_ms,
        ));
        self.ema_slow = Some(Self::update_ema(
            self.ema_slow,
            price,
            dt_ms,
            cfg.coinbase_ema_slow_ms,
        ));

        if let Some(prev) = prev_price {
            let move_usd = (price - prev).abs();
            self.vol_ema_usd = Some(Self::update_ema(
                self.vol_ema_usd,
                move_usd,
                dt_ms,
                cfg.coinbase_vol_ema_ms,
            ));
        }

        self.last_update_ms = Some(ts_ms);

        let push = self
            .samples
            .back()
            .map(|s| s.ts_ms != ts_ms && (s.price - price).abs() >= 0.005)
            .unwrap_or(true);
        if push {
            self.samples.push_back(CoinbaseSample { ts_ms, price });
        }

        let cutoff = ts_ms.saturating_sub(cfg.coinbase_history_ms);
        while let Some(front) = self.samples.front() {
            if front.ts_ms < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        true
    }

    pub fn apply_ticker(
        &mut self,
        cfg: &Config,
        ts_ms: u64,
        last_trade_price: f64,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
        best_bid_qty: Option<f64>,
        best_ask_qty: Option<f64>,
    ) -> bool {
        let before = self.snapshot();

        self.last_trade_price = Some(last_trade_price);
        if best_bid.is_some() {
            self.best_bid = best_bid;
        }
        if best_ask.is_some() {
            self.best_ask = best_ask;
        }
        if best_bid_qty.is_some() {
            self.best_bid_qty = best_bid_qty;
        }
        if best_ask_qty.is_some() {
            self.best_ask_qty = best_ask_qty;
        }

        let _ = self.refresh_price_state(cfg, ts_ms);
        let after = self.snapshot();
        before.best_bid != after.best_bid
            || before.best_ask != after.best_ask
            || before.last_trade_price != after.last_trade_price
            || before.microprice != after.microprice
    }

    pub fn apply_level2_snapshot(
        &mut self,
        cfg: &Config,
        ts_ms: u64,
        updates: &[(CoinbaseBookSide, f64, f64)],
    ) -> bool {
        self.bids.clear();
        self.asks.clear();
        self.apply_level2_update(cfg, ts_ms, updates)
    }

    pub fn apply_level2_update(
        &mut self,
        cfg: &Config,
        ts_ms: u64,
        updates: &[(CoinbaseBookSide, f64, f64)],
    ) -> bool {
        let before = self.snapshot();

        for (side, price, qty) in updates {
            let Some(key) = Self::price_to_key(*price) else {
                continue;
            };
            let book = match side {
                CoinbaseBookSide::Bid => &mut self.bids,
                CoinbaseBookSide::Ask => &mut self.asks,
            };
            if *qty <= 0.0 {
                book.remove(&key);
            } else {
                book.insert(key, *qty);
            }
        }

        self.choose_top_of_book();
        let _ = self.refresh_price_state(cfg, ts_ms);
        let after = self.snapshot();
        before.best_bid != after.best_bid
            || before.best_ask != after.best_ask
            || before.best_bid_qty != after.best_bid_qty
            || before.best_ask_qty != after.best_ask_qty
            || before.microprice != after.microprice
    }

    pub fn record_heartbeat(&mut self, ts_ms: u64, heartbeat_counter: Option<u64>) {
        self.last_heartbeat_ms = Some(ts_ms);
        if heartbeat_counter.is_some() {
            self.heartbeat_counter = heartbeat_counter;
        }
    }

    pub fn snapshot(&self) -> CoinbaseSnapshot {
        CoinbaseSnapshot {
            product_id: self.product_id.clone(),
            last_trade_price: self.last_trade_price,
            best_bid: self.best_bid,
            best_ask: self.best_ask,
            best_bid_qty: self.best_bid_qty,
            best_ask_qty: self.best_ask_qty,
            microprice: self.microprice,
            ema_fast: self.ema_fast,
            ema_slow: self.ema_slow,
            vol_ema_usd: self.vol_ema_usd,
            last_update_ms: self.last_update_ms,
            last_heartbeat_ms: self.last_heartbeat_ms,
            heartbeat_counter: self.heartbeat_counter,
            samples: self.samples.iter().cloned().collect(),
        }
    }
}
