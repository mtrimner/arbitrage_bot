use crate::state::Shared;
use crate::state::{book::Book, orders::Orders, position::Position};
use crate::types::{RestingHint, Side};

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Accumulate,
    Hedge,
    Balance,
}

#[derive(Debug)]
pub struct Market {
    // UTC epoch seconds
    pub open_ts: Option<i64>,
    pub close_ts: Option<i64>,
    pub strike_price: Option<f64>,

    pub book: Book,

    pub pos: Position,
    pub orders: Orders,

    pub resting_yes: Option<RestingHint>,
    pub resting_no: Option<RestingHint>,

    // Cooldowns for takers so we don’t spam.
    pub last_taker_yes: Option<Instant>,
    pub last_taker_no: Option<Instant>,
    pub last_no_order_reason: Option<&'static str>,
    pub last_no_order_reason_ts: Option<Instant>,
    pub last_signal_log_ts: Option<Instant>,
    pub last_signal_log_fair_cents: Option<u8>,

    pub mode: Mode,
}

impl Market {
    pub fn new() -> Self {
        Self {
            open_ts: None,
            close_ts: None,
            strike_price: None,
            book: Book::default(),
            pos: Position::default(),
            orders: Orders::default(),
            resting_yes: None,
            resting_no: None,
            last_taker_yes: None,
            last_taker_no: None,
            last_no_order_reason: None,
            last_no_order_reason_ts: None,
            last_signal_log_ts: None,
            last_signal_log_fair_cents: None,
            mode: Mode::Accumulate,
        }
    }

    pub fn resting_hint_mut(&mut self, side: Side) -> &mut Option<RestingHint> {
        match side {
            Side::Yes => &mut self.resting_yes,
            Side::No => &mut self.resting_no,
        }
    }

    pub fn resting_hint(&self, side: Side) -> &Option<RestingHint> {
        match side {
            Side::Yes => &self.resting_yes,
            Side::No => &self.resting_no,
        }
    }
}

#[derive(Debug)]
pub struct TickerState {
    pub mkt: RwLock<Market>,

    pub dirty: AtomicBool,
}

impl TickerState {
    pub fn new(_ticker: String) -> Self {
        Self {
            mkt: RwLock::new(Market::new()),
            dirty: AtomicBool::new(true),
        }
    }

    pub fn touch(&self, shared: &Shared) {
        self.mark_dirty();
        shared.notify.notify_one();
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}
