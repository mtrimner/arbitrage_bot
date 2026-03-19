pub mod book;
pub mod coinbase;
pub mod orders;
pub mod position;
pub mod ticker;

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

use coinbase::CoinbaseState;
use ticker::TickerState;

#[derive(Clone, Debug)]
pub struct Shared {
    pub tickers: Arc<DashMap<String, Arc<TickerState>>>,
    pub notify: Arc<Notify>,
    pub coinbase: Arc<RwLock<CoinbaseState>>,
}

impl Shared {
    pub fn new(tickers: Vec<String>, coinbase_product_id: String) -> Self {
        let map = DashMap::new();
        for t in tickers {
            map.insert(t.clone(), Arc::new(TickerState::new(t)));
        }
        Self {
            tickers: Arc::new(map),
            notify: Arc::new(Notify::new()),
            coinbase: Arc::new(RwLock::new(CoinbaseState::new(coinbase_product_id))),
        }
    }

    pub fn ensure_ticker(&self, ticker: &str) -> Arc<TickerState> {
        if let Some(existing) = self.tickers.get(ticker) {
            return existing.value().clone();
        }
        let ts = Arc::new(TickerState::new(ticker.to_string()));
        self.tickers.insert(ticker.to_string(), ts.clone());
        ts
    }

    pub fn remove_ticker(&self, ticker: &str) {
        self.tickers.remove(ticker);
    }

    pub fn touch_all(&self) {
        for item in self.tickers.iter() {
            item.value().mark_dirty();
        }
        self.notify.notify_one();
    }
}
