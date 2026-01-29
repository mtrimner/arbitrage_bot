use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::Config;

/// Simple EMA helper.
///
/// τ (“tau”) is the time constant:
/// - small τ => EMA reacts quickly
/// - large τ => EMA reacts slowly
///
/// Update formula:
///   α = 1 - exp(-Δt / τ)
///   ema = ema + α * (x - ema)
#[derive(Debug, Clone)]
pub struct Ema {
    pub value: f64,
    pub initialized: bool,
}

impl Default for Ema {
    fn default() -> Self {
        Self {
            value: 0.0,
            initialized: false,
        }
    }
}

impl Ema {
    pub fn update(&mut self, x: f64, dt: Duration, tau: Duration) {
        if !self.initialized {
            self.value = x;
            self.initialized = true;
            return;
        }
        let dt_s = dt.as_secs_f64().max(0.000_001);
        let tau_s = tau.as_secs_f64().max(0.000_001);

        // α = 1 - exp(-dt/tau)
        let alpha = 1.0 - (-dt_s / tau_s).exp();
        self.value += alpha * (x - self.value);
    }
}

/// Holds smoothed “microstructure pressure” features.
#[derive(Debug, Clone)]
pub struct FlowState {
    pub book_imb_ema: Ema,   // from orderbook depth
    pub trade_flow_ema: Ema, // from executed trades
    pub delta_flow_ema: Ema, // from orderbook deltas (adds/cancels near top)
    pub score_ema: Ema,      // final combined score smoothing

    pub last_book_at: Option<Instant>,
    pub last_trade_at: Option<Instant>,
    pub last_delta_at: Option<Instant>,
    pub last_score_at: Option<Instant>,

    // For “confidence scaling”: count recent events.
    pub trade_times: VecDeque<Instant>,
    pub delta_times: VecDeque<Instant>,
}

impl Default for FlowState {
    fn default() -> Self {
        Self {
            book_imb_ema: Ema::default(),
            trade_flow_ema: Ema::default(),
            delta_flow_ema: Ema::default(),
            score_ema: Ema::default(),

            last_book_at: None,
            last_trade_at: None,
            last_delta_at: None,
            last_score_at: None,

            trade_times: VecDeque::with_capacity(512),
            delta_times: VecDeque::with_capacity(1024),
        }
    }
}

impl FlowState {
    fn prune_times(times: &mut VecDeque<Instant>, window: Duration, now: Instant) {
        while let Some(front) = times.front().copied() {
            if now.duration_since(front) > window {
                times.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn trade_count_recent(&mut self, cfg: &Config, now: Instant) -> usize {
        let window = Duration::from_millis(cfg.rate_window_ms);
        Self::prune_times(&mut self.trade_times, window, now);
        self.trade_times.len()
    }

    pub fn delta_count_recent(&mut self, cfg: &Config, now: Instant) -> usize {
        let window = Duration::from_millis(cfg.rate_window_ms);
        Self::prune_times(&mut self.delta_times, window, now);
        self.delta_times.len()
    }

    pub fn on_book_imbalance(&mut self, cfg: &Config, raw_imb: f64, now: Instant) {
        let tau = Duration::from_millis(cfg.tau_book_ms);
        let dt = self
            .last_book_at
            .map(|t| now.duration_since(t))
            .unwrap_or(Duration::from_millis(cfg.tick_ms));
        self.book_imb_ema.update(raw_imb.clamp(-1.0, 1.0), dt, tau);
        self.last_book_at = Some(now);
    }

    pub fn on_trade_flow(&mut self, cfg: &Config, raw_flow: f64, now: Instant) {
        let tau = Duration::from_millis(cfg.tau_trade_ms);
        let dt = self
            .last_trade_at
            .map(|t| now.duration_since(t))
            .unwrap_or(Duration::from_millis(cfg.tick_ms));
        self.trade_flow_ema.update(raw_flow.clamp(-1.0, 1.0), dt, tau);
        self.last_trade_at = Some(now);

        self.trade_times.push_back(now);
        Self::prune_times(
            &mut self.trade_times,
            Duration::from_millis(cfg.rate_window_ms),
            now,
        );
    }

    pub fn on_delta_flow(&mut self, cfg: &Config, raw_flow: f64, now: Instant) {
        let tau = Duration::from_millis(cfg.tau_delta_ms);
        let dt = self
            .last_delta_at
            .map(|t| now.duration_since(t))
            .unwrap_or(Duration::from_millis(cfg.tick_ms));
        self.delta_flow_ema.update(raw_flow.clamp(-1.0, 1.0), dt, tau);
        self.last_delta_at = Some(now);

        self.delta_times.push_back(now);
        Self::prune_times(
            &mut self.delta_times,
            Duration::from_millis(cfg.rate_window_ms),
            now,
        );
    }

    /// Score EMA on top of the combined score.
    pub fn on_score(&mut self, cfg: &Config, raw_score: f64, now: Instant) {
        let tau = Duration::from_millis(cfg.tau_score_ms);
        let dt = self
            .last_score_at
            .map(|t| now.duration_since(t))
            .unwrap_or(Duration::from_millis(cfg.tick_ms));
        self.score_ema.update(raw_score.clamp(-1.0, 1.0), dt, tau);
        self.last_score_at = Some(now);
    }
}
