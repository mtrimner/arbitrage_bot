use crate::state::{book::Book, orders::Orders, position::Position};
use crate::types::{RestingHint, Side};
use crate::state::Shared;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
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

    pub book: Book,

    pub pos: Position,
    pub orders: Orders,

    pub resting_yes: Option<RestingHint>,
    pub resting_no: Option<RestingHint>,

    // Cooldowns for takers so we don’t spam.
    pub last_taker_yes: Option<Instant>,
    pub last_taker_no: Option<Instant>,

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

    #[inline]
    pub fn has_pair(&self) -> bool {
        self.pos.yes_qty > 0 && self.pos.no_qty > 0
    }

    #[inline]
    pub fn qty_for(&self, side: Side) -> i64 {
        match side {
            Side::Yes => self.pos.yes_qty,
            Side::No => self.pos.no_qty,
        }
    }

    #[inline]
    pub fn avg_cc_for(&self, side: Side) -> Option<i64> {
        match side {
            Side::Yes => self.pos.avg_yes_cc(),
            Side::No => self.pos.avg_no_cc(),
        }
    }

    #[inline]
    pub fn last_taker(&self, side: Side) -> Option<Instant> {
        match side {
            Side::Yes => self.last_taker_yes,
            Side::No => self.last_taker_no,
        }
    }

    #[inline]
    pub fn set_last_taker(&mut self, side: Side, t: Instant) {
        match side {
            Side::Yes => self.last_taker_yes = Some(t),
            Side::No => self.last_taker_no = Some(t),
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
