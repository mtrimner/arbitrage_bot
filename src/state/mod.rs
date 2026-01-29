pub mod ticker;
pub mod position;
pub mod book;
pub mod orders;
pub mod flow;

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Notify;

use ticker::TickerState;

#[derive(Clone, Debug)]
pub struct Shared {
    pub tickers: Arc<DashMap<String, Arc<TickerState>>>,
    pub notify: Arc<Notify>,
}

impl Shared {
    pub fn new(tickers: Vec<String>) -> Self {
        let map = DashMap::new();
        for t in tickers {
            map.insert(t.clone(), Arc::new(TickerState::new(t)));
        }
        Self {
            tickers: Arc::new(map),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Ensure a ticker exists in the shared map (insert if missing).
    pub fn ensure_ticker(&self, ticker: &str) -> Arc<TickerState> {
        if let Some(existing) = self.tickers.get(ticker) {
            return existing.value().clone();
        }
        let ts = Arc::new(TickerState::new(ticker.to_string()));
        self.tickers.insert(ticker.to_string(), ts.clone());
        ts
    }

    /// Remove a ticker from Shared (engine will stop iterating it).
    pub fn remove_ticker(&self, ticker: &str) {
        self.tickers.remove(ticker);
    }
}
