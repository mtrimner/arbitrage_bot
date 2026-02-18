use std::time::Instant;

pub const CC_PER_CENT: i64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Yes,
    No,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Yes => Side::No,
            Side::No => Side::Yes,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Yes => "yes",
            Side::No => "no",
        }
    }
}

// #[derive(Debug, Clone)]
// pub struct TradeLite {
//     pub ts: i64,
//     pub taker_side: Side,
//     pub count: i64,
//     pub yes_price: u8,
//     pub no_price: u8,
// }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    Ioc,
    Gtc,
}

#[derive(Debug, Clone)]
pub enum ExecCommand {
    PlaceOrder {
        ticker: String,
        side: Side,
        price_cents: u8,
        qty: u64,
        tif: Tif,
        post_only: bool,
        client_order_id: uuid::Uuid,
    },
    CancelOrder {
        ticker: String,
        order_id: String,
    },
}

/// Tracks a resting order we believe is live (or pending ack).
///
/// We keep this so the engine can:
/// - avoid placing duplicates
/// - decide when to cancel/replace
/// - avoid churn (min_resting_life_ms)
#[derive(Debug, Clone)]
pub struct RestingHint {
    pub side: Side,
    pub price_cents: u8,
    pub created_at: Instant,

    // If we’ve sent a cancel, we set this to avoid re-sending cancel every tick.
    pub cancel_requested_at: Option<Instant>,

    pub client_order_id: uuid::Uuid,

    // Filled in by executor once HTTP create_order returns the exchange order id.
    pub order_id: Option<String>,

    // PAPER_SIM: qty that was already resting at this price when we joined.
    pub queue_ahead: i64,
}

/// Commands sent from MarketManager -> WS task
/// so the WS task can update subscriptions using sids.
#[derive(Debug, Clone)]
pub enum WsMarketCommand {
    /// Add/remove market tickers across the existing channel subscriptions.
    /// We do add first, then delete, to reduce gaps.
    UpdateMarkets {
        add: Vec<String>,
        remove: Vec<String>,
    },
}