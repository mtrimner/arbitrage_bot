use std::collections::HashMap;
use std::time::Instant;

use crate::types::{Side, Tif};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    PendingAck,
    Resting,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct OrderRec {
    pub ticker: String,
    pub side: Side,
    pub price_cents: u8,
    pub qty: u64,
    pub tif: Tif,
    pub post_only: bool,

    pub order_id: Option<String>,
    pub client_order_id: uuid::Uuid,

    pub status: OrderStatus,
    pub created_at: Instant,

    pub filled_qty: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Orders {
    pub by_client: HashMap<uuid::Uuid, OrderRec>,
    pub by_order: HashMap<String, uuid::Uuid>,
}

impl Orders {
    pub fn link_order_id_if_missing(&mut self, client_id: uuid::Uuid, order_id: &str) {
        // If we don't know this client_id do nothing
        let Some(rec) = self.by_client.get_mut(&client_id) else { return; };

        // If order_id is missing, set it.
        if rec.order_id.as_deref() == Some(order_id) {
            rec.order_id = Some(order_id.to_string());
        }

        // Always ensure reverse map.
        self.by_order.insert(order_id.to_string(), client_id);
    }

    /// Apply a fill using client_id (best path because UserFill gives client_order_id).
    /// Returns:
    /// - Some(true)  => now fully filled
    /// - Some(false) => partial fill
    /// - None        => unknown client_id
    pub fn on_fill_by_client(&mut self, client_id: uuid::Uuid, fill_qty: u64) -> Option<bool> {
        let rec = self.by_client.get_mut(&client_id)?;

        rec.filled_qty = rec.filled_qty.saturating_add(fill_qty);

        if rec.filled_qty >= rec.qty {
            rec.status = OrderStatus::Filled;
            Some(true)
        } else {
            Some(false)
        }
    }

    /// Apply a fill by exchange order_id (fallback path).
    pub fn on_fill_by_order(&mut self, order_id: &str, fill_qty: u64) -> Option<bool> {
        let client_id = *self.by_order.get(order_id)?;
        self.on_fill_by_client(client_id, fill_qty)
    }

    pub fn on_fill (&mut self, order_id: &str, fill_qty: u64) -> Option<bool> {
        let client_id = *self.by_order.get(order_id)?;
        let rec = self.by_client.get_mut(&client_id)?;

        rec.filled_qty = rec.filled_qty.saturating_add(fill_qty);

        if rec.filled_qty >= rec.qty {
            rec.status = OrderStatus::Filled;
            Some(true)
        } else {
            // Leave status as Resting/PendingAck; Add PartialFilled status later if needed
            Some(false)
        }
    }

    pub fn insert_pending(&mut self, rec: OrderRec) {
        self.by_client.insert(rec.client_order_id, rec);
    }

    pub fn link_order_id(&mut self, client_id: uuid::Uuid, order_id: &str) {
        if let Some(o) = self.by_client.get_mut(&client_id) {
            o.order_id = Some(order_id.to_string());
            self.by_order.insert(order_id.to_string(), client_id);
        }
    }

    pub fn set_status_by_client(&mut self, client_id: uuid::Uuid, st: OrderStatus) {
        if let Some(o) = self.by_client.get_mut(&client_id) {
            o.status = st;
        }
    }

    pub fn set_status_by_order(&mut self, order_id: &str, st: OrderStatus) {
        if let Some(client_id) = self.by_order.get(order_id).copied() {
            self.set_status_by_client(client_id, st);
        }
    }

    pub fn client_for_order(&self, order_id: &str) -> Option<uuid::Uuid> {
        self.by_order.get(order_id).copied()
    }
}
