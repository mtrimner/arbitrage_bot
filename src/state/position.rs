use crate::types::{CC_PER_CENT, Side};

const DOLLAR_CC: i64 = 100 * CC_PER_CENT;

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub yes_qty: i64,
    pub no_qty: i64,
    pub yes_cost_cc: i64,
    pub no_cost_cc: i64,
    pub opening_yes_price_cents: Option<u8>,
    pub opening_no_price_cents: Option<u8>,
}

impl Position {
    pub fn avg_yes_cc(&self) -> Option<i64> {
        if self.yes_qty <= 0 {
            None
        } else {
            Some(self.yes_cost_cc / self.yes_qty)
        }
    }

    pub fn avg_no_cc(&self) -> Option<i64> {
        if self.no_qty <= 0 {
            None
        } else {
            Some(self.no_cost_cc / self.no_qty)
        }
    }

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

    pub fn locked_floor_cc(&self) -> i64 {
        self.yes_qty.min(self.no_qty) * DOLLAR_CC - self.yes_cost_cc - self.no_cost_cc
    }

    pub fn total_cost_cc(&self) -> i64 {
        self.yes_cost_cc.saturating_add(self.no_cost_cc)
    }

    pub fn max_avg_price_to_balance_cc(&self, missing_side: Side, buffer_cc: i64) -> Option<i64> {
        match missing_side {
            Side::Yes => {
                if self.no_qty <= self.yes_qty {
                    return Some(DOLLAR_CC);
                }
                let need = self.no_qty - self.yes_qty;
                if need <= 0 {
                    return Some(DOLLAR_CC);
                }
                Some(((self.no_qty * DOLLAR_CC - buffer_cc) - self.total_cost_cc()) / need)
            }
            Side::No => {
                if self.yes_qty <= self.no_qty {
                    return Some(DOLLAR_CC);
                }
                let need = self.yes_qty - self.no_qty;
                if need <= 0 {
                    return Some(DOLLAR_CC);
                }
                Some(((self.yes_qty * DOLLAR_CC - buffer_cc) - self.total_cost_cc()) / need)
            }
        }
    }

    pub fn apply_fill(&mut self, side: Side, price_cents: u8, qty: i64) {
        let add_cc = (price_cents as i64) * CC_PER_CENT * qty;
        match side {
            Side::Yes => {
                if qty > 0 && self.yes_qty <= 0 && self.opening_yes_price_cents.is_none() {
                    self.opening_yes_price_cents = Some(price_cents);
                }
                self.yes_qty += qty;
                self.yes_cost_cc += add_cc;
            }
            Side::No => {
                if qty > 0 && self.no_qty <= 0 && self.opening_no_price_cents.is_none() {
                    self.opening_no_price_cents = Some(price_cents);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_opening_fill_price_once_per_side() {
        let mut pos = Position::default();

        pos.apply_fill(Side::Yes, 49, 1);
        pos.apply_fill(Side::Yes, 51, 1);
        pos.apply_fill(Side::No, 50, 2);
        pos.apply_fill(Side::No, 48, 1);

        assert_eq!(pos.opening_yes_price_cents, Some(49));
        assert_eq!(pos.opening_no_price_cents, Some(50));
    }
}
