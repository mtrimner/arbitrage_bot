use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{info, warn};

use std::sync::Arc;

use kalshi_rs::KalshiClient;

use crate::exec::{http, paper};
use crate::state::orders::OrderStatus;
use crate::state::Shared;
use crate::types::{ExecCommand, Side, Tif};
use crate::config::{Config, ExecMode};

pub async fn run_exec(
    cfg: Config,
    client: Arc<KalshiClient>,
    shared: Shared,
    mut rx: mpsc::Receiver<ExecCommand>,
) -> Result<()> {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ExecCommand::PlaceOrder {
                ticker,
                side,
                price_cents,
                qty,
                tif,
                post_only,
                client_order_id,
            } => {

                if cfg.exec_mode.is_paper() {
                    paper::paper_place(
                        &shared, &ticker, side, price_cents, qty, tif, post_only,
                        client_order_id, cfg.paper_reject_postonly_cross
                    ).await;
                    continue;
                }

                let res = http::place(
                    &client,
                    &ticker,
                    side,
                    price_cents,
                    qty,
                    tif,
                    &client_order_id.to_string(),
                    post_only,
                )
                .await;

                match res {
                    Ok(resp) => {
                        // Kalshi returns an Order with order_id + status.
                        let order_id = resp.order.order_id.clone();
                        let status = resp.order.status.clone();

                        info!(
                            "placed order side={:?} tif={:?} post_only={} price={} id={} status={}",
                            side, tif, post_only, price_cents, order_id, status
                        );

                        if let Some(ts) = shared.tickers.get(&ticker) {
                            let mut g = ts.mkt.write().await;

                            // Link exchange order_id to our client_order_id
                            g.orders.link_order_id(client_order_id, &order_id);

                            // Update local status
                            let st = match status.as_str() {
                                "resting" => OrderStatus::Resting,
                                "canceled" => OrderStatus::Canceled,
                                "filled" | "executed" => OrderStatus::Filled,
                                _ => OrderStatus::Resting, // conservative
                            };
                            g.orders.set_status_by_client(client_order_id, st);

                            // If this was meant to be a resting order, fill in the hint's order_id.
                            if tif == Tif::Gtc && post_only {
                                if let Some(h) = g.resting_hint_mut(side).as_mut() {
                                    if h.client_order_id == client_order_id {
                                        h.order_id = Some(order_id.clone());
                                    }
                                }
                            }

                            // If it was IOC, we don’t keep any resting hint.
                            // Fills will come through websocket (fill channel).
                            ts.mark_dirty();
                            shared.notify.notify_one();
                        }
                    }
                    Err(e) => {
                        warn!("place failed: {e:?}");
                        if let Some(ts) = shared.tickers.get(&ticker) {
                            let mut g = ts.mkt.write().await;
                            g.orders.set_status_by_client(client_order_id, OrderStatus::Rejected);

                            // If we thought this was resting, clear the hint so engine can try again.
                            if let Some(h) = g.resting_hint(side).clone() {
                                if h.client_order_id == client_order_id {
                                    *g.resting_hint_mut(side) = None;
                                }
                            }

                            ts.mark_dirty();
                            shared.notify.notify_one();
                        }
                    }
                }
            }

            ExecCommand::CancelOrder { ticker, order_id } => {
                if cfg.exec_mode.is_paper() {
                    paper::paper_cancel(&shared, &ticker, &order_id).await;
                    continue;
                }

                let res = http::cancel(&client, &order_id).await;
                match res {
                    Ok(_) => {
                        info!("canceled order_id={}", order_id);

                        if let Some(ts) = shared.tickers.get(&ticker) {
                            let mut g = ts.mkt.write().await;

                            g.orders.set_status_by_order(&order_id, OrderStatus::Canceled);

                            // Clear any resting hint that matches this order_id.
                            for side in [Side::Yes, Side::No] {
                                if let Some(h) = g.resting_hint(side).clone() {
                                    if h.order_id.as_deref() == Some(order_id.as_str()) {
                                        *g.resting_hint_mut(side) = None;
                                    }
                                }
                            }

                            ts.mark_dirty();
                            shared.notify.notify_one();
                        }
                    }
                    Err(e) => {
                        warn!("cancel failed: {e:?}");
                        // On cancel failure, we just leave hint intact;
                        // engine will retry after cfg.cancel_retry_ms due to cancel_requested_at timestamp.
                    }
                }
            }
        }
    }

    Ok(())
}
