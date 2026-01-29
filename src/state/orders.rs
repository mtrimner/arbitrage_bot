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
}

#[derive(Debug, Default, Clone)]
pub struct Orders {
    pub by_client: HashMap<uuid::Uuid, OrderRec>,
    pub by_order: HashMap<String, uuid::Uuid>,
}

impl Orders {
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
