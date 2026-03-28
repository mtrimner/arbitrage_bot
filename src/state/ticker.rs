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

#[derive(Debug, Clone)]
pub struct PairPlan {
    pub yes_target_price_cents: u8,
    pub no_target_price_cents: u8,
    pub budget_cc: i64,
    pub first_side: Side,
    pub first_fill_side: Option<Side>,
    pub first_fill_price_cents: Option<u8>,
    pub created_at: Instant,
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
    pub pair_plan: Option<PairPlan>,

    // Cooldowns for takers so we don’t spam.
    pub last_taker_yes: Option<Instant>,
    pub last_taker_no: Option<Instant>,
    pub imbalance_since: Option<Instant>,
    pub last_no_order_reason: Option<&'static str>,
    pub last_no_order_reason_ts: Option<Instant>,
    pub last_signal_log_ts: Option<Instant>,
    pub last_pair_open_log_ts: Option<Instant>,

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
            pair_plan: None,
            last_taker_yes: None,
            last_taker_no: None,
            imbalance_since: None,
            last_no_order_reason: None,
            last_no_order_reason_ts: None,
            last_signal_log_ts: None,
            last_pair_open_log_ts: None,
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

    pub fn set_pair_plan(
        &mut self,
        yes_price: u8,
        no_price: u8,
        first_side: Side,
        budget_cc: i64,
        now: Instant,
    ) {
        self.pair_plan = Some(PairPlan {
            yes_target_price_cents: yes_price,
            no_target_price_cents: no_price,
            budget_cc,
            first_side,
            first_fill_side: None,
            first_fill_price_cents: None,
            created_at: now,
        });
    }

    pub fn apply_tracked_fill(&mut self, side: Side, price_cents: u8, qty: i64) {
        let mut clear_plan = false;
        if qty > 0 {
            if let Some(plan) = self.pair_plan.as_mut() {
                match plan.first_fill_side {
                    None => {
                        plan.first_fill_side = Some(side);
                        plan.first_fill_price_cents = Some(price_cents);
                    }
                    Some(first_side) if first_side != side => {
                        clear_plan = true;
                    }
                    Some(_) => {}
                }
            }
        }

        self.pos.apply_fill(side, price_cents, qty);

        if clear_plan {
            self.pair_plan = None;
        }
    }

    pub fn clear_pair_plan_if_inactive(&mut self) {
        let Some(plan) = self.pair_plan.as_ref() else {
            return;
        };
        if plan.first_fill_side.is_some() {
            return;
        }

        let yes_active = self.resting_yes.as_ref().is_some_and(|h| {
            h.cancel_requested_at.is_none() && h.price_cents == plan.yes_target_price_cents
        });
        let no_active = self.resting_no.as_ref().is_some_and(|h| {
            h.cancel_requested_at.is_none() && h.price_cents == plan.no_target_price_cents
        });

        if !yes_active && !no_active {
            self.pair_plan = None;
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
