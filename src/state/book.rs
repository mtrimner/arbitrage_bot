use crate::types::Side;

#[derive(Debug, Clone)]
pub struct Book {
    // Each index is a price in cents (0..=100), value is quantity resting.
    pub yes_bids: [i64; 101],
    pub no_bids: [i64; 101],
    pub last_seq: i64,
}

impl Default for Book {
    fn default() -> Self {
        Self {
            yes_bids: [0; 101],
            no_bids: [0; 101],
            last_seq: -1,
        }
    }
}

impl Book {
    pub fn reset(&mut self, seq: i64, yes: &[(u8, i64)], no: &[(u8, i64)]) {
        self.yes_bids = [0; 101];
        self.no_bids = [0; 101];
        for &(p, q) in yes {
            self.yes_bids[p as usize] = q.max(0);
        }
        for &(p, q) in no {
            self.no_bids[p as usize] = q.max(0);
        }
        self.last_seq = seq;
    }

    pub fn apply_delta(&mut self, seq: i64, side: Side, price: u8, delta: i64) -> bool {
        // If seq continuity breaks, caller should resync (REST orderbook or re-subscribe).
        if self.last_seq >= 0 && seq != self.last_seq + 1 {
            return false;
        }
        let idx = price as usize;
        if idx > 100 {
            return true;
        }

        let arr = match side {
            Side::Yes => &mut self.yes_bids,
            Side::No => &mut self.no_bids,
        };
        arr[idx] = (arr[idx] + delta).max(0);
        self.last_seq = seq;
        true
    }

    pub fn best_bid(&self, side: Side) -> Option<u8> {
        let arr = match side {
            Side::Yes => &self.yes_bids,
            Side::No => &self.no_bids,
        };
        for p in (0..=100).rev() {
            if arr[p] > 0 {
                return Some(p as u8);
            }
        }
        None
    }

    // In a binary market, buying YES at the ask is equivalent to buying NO at its bid:
    // yes_ask ~= 100 - best_no_bid, and vice versa.
    pub fn implied_ask(&self, side: Side) -> Option<u8> {
        match side {
            Side::Yes => self.best_bid(Side::No).map(|b| 100u8.saturating_sub(b)),
            Side::No => self.best_bid(Side::Yes).map(|b| 100u8.saturating_sub(b)),
        }
    }

    // True if a new BUY order at `price` would cross the (implied) ask right now.
    // Useful to ensure post_only orders won't be rejected at entry time.
    pub fn crosses_ask(&self, side: Side, price: u8) -> bool {
        self.implied_ask(side).map(|ask| price >= ask).unwrap_or(false)
    }
}
