use anyhow::Result;
use kalshi_rs::KalshiClient;
use kalshi_rs::portfolio::models::{CreateOrderRequest, CreateOrderResponse};

use crate::types::{Side, Tif};

fn tif_str(t: Tif) -> &'static str {
    match t {
        Tif::Ioc => "ioc",
        Tif::Gtc => "gtc",
    }
}

/// Place a limit order.
///
/// NOTE: CreateOrderRequest does NOT implement Default in kalshi-rs 0.2.1,
/// so we must construct the struct with all fields.
pub async fn place(
    client: &KalshiClient,
    ticker: &str,
    side: Side,
    price_cents: u8,
    qty: u64,
    tif: Tif,
    client_order_id: &str,
    post_only: bool,
) -> Result<CreateOrderResponse> {
    let (yes_price, no_price) = match side {
        Side::Yes => (Some(price_cents as u64), None),
        Side::No => (None, Some(price_cents as u64)),
    };

    let req = CreateOrderRequest {
        ticker: ticker.to_string(),
        side: side.as_str().to_string(),
        action: "buy".to_string(),
        count: qty,

        client_order_id: Some(client_order_id.to_string()),
        type_: Some("limit".to_string()),
        yes_price,
        no_price,

        yes_price_dollars: None,
        no_price_dollars: None,
        expiration_ts: None,
        time_in_force: Some(tif_str(tif).to_string()),
        buy_max_cost: None,

        post_only: Some(post_only),
        reduce_only: None,
        self_trade_prevention_type: None,
        order_group_id: None,
        cancel_order_on_pause: None,
    };

    Ok(client.create_order(&req).await?)
}

pub async fn cancel(client: &KalshiClient, order_id: &str) -> Result<()> {
    client.cancel_order(order_id.to_string()).await?;
    Ok(())
}
