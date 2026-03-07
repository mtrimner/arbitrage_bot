use std::time::{Duration, SystemTime, UNIX_EPOCH};
use cbadv::{
    models::websocket::{Channel, EndpointStream, Events, Message},
    types::CbResult,
    WebSocketClientBuilder,
};
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct CoinbasePrice {
    pub product_id: String, // "BTC-USD"
    pub price: f64, // last ticker price
    pub ts_ms: u64, // local receive timestamp (ms since unix epoch)
}

impl CoinbasePrice {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64
    }
}

/// Spawns a Coinbase websocket client subscribed to `Channel::Ticker` for `product_id`.
///
/// Returns a `watch::Receiver` that always holds the most recent price snapshot.
pub fn spawn_coinbase_ticker(product_id: String) -> watch::Receiver<CoinbasePrice> {
    let (tx, rx) = watch::channel(CoinbasePrice {
        product_id: product_id.clone(),
        price: f64::NAN,
        ts_ms: 0,
    });

    tokio::spawn(async move {
        // Simple forever-restart loop so transient errors don't kill the whole bot.
        loop {
            match run_coinbase_ticker_once(product_id.clone(), tx.clone()).await {
                Ok(()) => {
                    // This would be unusual (we expect to run forever); restart anyway.
                    warn!("coinbase ws exited cleanly; restarting");
                }
                Err(e) => {
                    warn!(err=%format!("{e:?}"), "coinbase ws error; restarting soon");
                }
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    });

    rx
}

async fn run_coinbase_ticker_once(
    product_id: String,
    tx: watch::Sender<CoinbasePrice>,
) -> CbResult<()> {
    // NOTE: We follow the cbadv websocket example pattern:
    // - build client
    // - connect() to get readers
    // - subscribe() to desired channels
    // - loop fetch_sync() to process messages
    let mut ws_client = WebSocketClientBuilder::new()
        .auto_reconnect(true)
        .max_retries(0) // 0 == unlimited (per cbadv convention)
        .build()?;

    let readers = ws_client.connect().await?;

    // Heartbeats are optional but handy to prove the socket is alive.
    ws_client.subscribe(&Channel::Heartbeats, &[]).await?;
    ws_client
        .subscribe(&Channel::Ticker, &[product_id.clone()])
        .await?;

    info!(product_id=%product_id, "coinbase ws connected  subscribed (ticker)");

    let mut stream: EndpointStream = readers.into();

    loop {
        // Poll up to N messages per tick; callback runs synchronously.
        // (Same approach as the cbadv websocket example.)
        let tx2 = tx.clone();
        let _ = ws_client.fetch_sync(&mut stream, 100, move |msg| {
            match msg {
                Ok(Message {
                    events: Events::Ticker(ticker_events),
                    ..
                }) => {
                    let now_ms = CoinbasePrice::now_ms();

                    // We subscribed to one product, but events can carry multiple updates.
                    for ev in ticker_events {
                        // Field name in cbadv: `tickers: Vec<TickerUpdate>`
                        for update in ev.tickers {
                            let _ = tx2.send(CoinbasePrice {
                                product_id: update.product_id,
                                price: update.price,
                                ts_ms: now_ms,
                            });
                        }
                    }
                }
                Ok(_) => {
                    // ignore other channels/events
                }
                Err(e) => {
                    // Keep going; auto_reconnect should handle most transient issues.
                    warn!(err=%format!("{e:?}"), "coinbase ws message error");
                }
            }
            Ok(())
        });

        // Yield to runtime; keep CPU low.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Optional: logs the ticker when it moves by at least `min_delta_usd`.
/// Useful for eyeballing lead/lag against Kalshi orderbook timestamps.
pub fn spawn_coinbase_logger(mut rx: watch::Receiver<CoinbasePrice>, min_delta_usd: f64) {
    tokio::spawn(async move {
        let mut last = None::<CoinbasePrice>;
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let cur = rx.borrow().clone();
            let should_log = match &last {
                None => true,
                Some(prev) => (cur.price - prev.price).abs() >= min_delta_usd,
            };
            if should_log {
                info!(
                    target: "coinbase",
                    product_id=%cur.product_id,
                    price=cur.price,
                    ts_ms=cur.ts_ms,
                    "coinbase ticker"
                );
                last = Some(cur);
            }
        }
    });
}