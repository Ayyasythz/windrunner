mod book;

use std::collections::BTreeMap;

pub use book::{BookConfig, BookSnapshot, OrderBook, StpMode};
use serde::{Deserialize, Serialize};
use windrunner_types::{CancelReason, Command, Event, MarketId, RejectReason};

// Ordered map: `expire_all` walks every market and its event order must be
// identical on replay.
#[derive(Default)]
pub struct Engine {
    books: BTreeMap<MarketId, OrderBook>,
}

#[derive(Serialize, Deserialize)]
pub struct EngineSnapshot {
    books: Vec<BookSnapshot>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_market(&mut self, market: MarketId, cfg: BookConfig) -> bool {
        if self.books.contains_key(&market) {
            return false;
        }
        self.books.insert(market, OrderBook::new(market, cfg));
        true
    }

    pub fn book(&self, market: MarketId) -> Option<&OrderBook> {
        self.books.get(&market)
    }

    pub fn books(&self) -> impl Iterator<Item = &OrderBook> {
        self.books.values()
    }

    pub fn apply(&mut self, cmd: Command, now: u64) -> Vec<Event> {
        match cmd {
            Command::Place(o) => match self.books.get_mut(&o.market) {
                Some(book) => book.place(o, now),
                None => vec![Event::Rejected {
                    order_id: o.id,
                    reason: RejectReason::UnknownMarket,
                }],
            },
            Command::Cancel {
                market,
                order_id,
                account,
            } => match self.books.get_mut(&market) {
                Some(book) => book.cancel(order_id, account),
                None => vec![Event::CancelRejected {
                    order_id,
                    reason: RejectReason::UnknownMarket,
                }],
            },
        }
    }

    pub fn cancel_all(&mut self, market: MarketId, reason: CancelReason) -> Vec<Event> {
        self.books
            .get_mut(&market)
            .map(|b| b.cancel_all(reason))
            .unwrap_or_default()
    }

    pub fn expire_all(&mut self, now: u64) -> Vec<Event> {
        self.books
            .values_mut()
            .flat_map(|b| b.expire(now))
            .collect()
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            books: self.books.values().map(OrderBook::snapshot).collect(),
        }
    }

    pub fn restore(snapshot: EngineSnapshot) -> Self {
        Engine {
            books: snapshot
                .books
                .into_iter()
                .map(|b| (b.market(), OrderBook::restore(b)))
                .collect(),
        }
    }
}
