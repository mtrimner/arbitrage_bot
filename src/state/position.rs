use crate::types::{Side, CC_PER_CENT};

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub yes_qty: i64,
    pub no_qty: i64,
    pub yes_cost_cc: i64,
    pub no_cost_cc: i64,
}

impl Position {
    pub fn avg_yes_cc(&self) -> Option<i64> {
        if self.yes_qty <= 0 {
            None
        } else {
            Some(self.yes_cost_cc / self.yes_qty)
        }
    }

    // BUGFIX: your original avg_no_cc checked yes_qty. It should check no_qty.
    pub fn avg_no_cc(&self) -> Option<i64> {
        if self.no_qty <= 0 {
            None
        } else {
            Some(self.no_cost_cc / self.no_qty)
        }
    }

    // avg_yes + avg_no in cc (cent-cents). Want < 10000 for "under $1 total".
    pub fn pair_cost_cc(&self) -> Option<i64> {
        Some(self.avg_yes_cc()? + self.avg_no_cc()?)
    }

    pub fn imbalance_ratio(&self) -> f64 {
        let diff = (self.yes_qty - self.no_qty).abs() as f64;
        let total = (self.yes_qty + self.no_qty).max(1) as f64;
        diff / total
    }

    pub fn is_balanced(&self) -> bool {
        self.yes_qty == self.no_qty
    }

    pub fn apply_fill(&mut self, side: Side, price_cents: u8, qty: i64) {
        let add_cc = (price_cents as i64) * CC_PER_CENT * qty;
        match side {
            Side::Yes => {
                self.yes_qty += qty;
                self.yes_cost_cc += add_cc;
            }
            Side::No => {
                self.no_qty += qty;
                self.no_cost_cc += add_cc;
            }
        }
    }

    pub fn simulate_buy(&self, side: Side, price_cents: u8, qty: i64) -> Position {
        let mut p = self.clone();
        p.apply_fill(side, price_cents, qty);
        p
    }
}
