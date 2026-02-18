use crate::state::{book::Book, orders::Orders, position::Position};
use crate::types::RestingHint;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
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

    pub book: Book,

    pub pos: Position,
    pub orders: Orders,

    pub resting_yes: Option<RestingHint>,
    pub resting_no: Option<RestingHint>,

    // Cooldowns for takers so we don’t spam.
    pub last_taker_yes: Option<std::time::Instant>,
    pub last_taker_no: Option<std::time::Instant>,

    pub mode: Mode,
}

impl Market {
    pub fn new() -> Self {
        Self {
            open_ts: None,
            close_ts: None,
            book: Book::default(),
            pos: Position::default(),
            orders: Orders::default(),
            resting_yes: None,
            resting_no: None,
            last_taker_yes: None,
            last_taker_no: None,
            mode: Mode::Accumulate,
        }
    }

    pub fn resting_hint_mut(&mut self, side: crate::types::Side) -> &mut Option<RestingHint> {
        match side {
            crate::types::Side::Yes => &mut self.resting_yes,
            crate::types::Side::No => &mut self.resting_no,
        }
    }

    pub fn resting_hint(&self, side: crate::types::Side) -> &Option<RestingHint> {
        match side {
            crate::types::Side::Yes => &self.resting_yes,
            crate::types::Side::No => &self.resting_no,
        }
    }
}

#[derive(Debug)]
pub struct TickerState {
    pub ticker: String,
    pub mkt: RwLock<Market>,

    pub dirty: AtomicBool,
    pub last_dirty_ns: AtomicI64,
}

impl TickerState {
    pub fn new(ticker: String) -> Self {
        Self {
            ticker,
            mkt: RwLock::new(Market::new()),
            dirty: AtomicBool::new(true),
            last_dirty_ns: AtomicI64::new(0),
        }
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}
