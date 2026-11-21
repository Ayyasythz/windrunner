mod sequencer;

use std::collections::BTreeMap;

pub use sequencer::{Applied, Sequencer};
use serde::{Deserialize, Serialize};
use windrunner_engine::{BookConfig, Engine, EngineSnapshot};
use windrunner_ledger::{Ledger, LedgerError};
use windrunner_types::{
    AccountId, CancelReason, Command, Event, MarketId, MarketInfo, MarketStatus, NewOrder, Outcome,
    RejectReason,
};

// The journal stores variants by position: only ever append new ones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    CreateMarket {
        market: MarketId,
        config: BookConfig,
    },
    Deposit {
        account: AccountId,
        amount: u64,
    },
    Withdraw {
        account: AccountId,
        amount: u64,
    },
    Order(Command),
    /// Expires GTD orders in every market.
    Tick,
    PauseMarket {
        market: MarketId,
    },
    ResumeMarket {
        market: MarketId,
    },
    /// Cancels every resting order and ends trading.
    CloseMarket {
        market: MarketId,
    },
    /// Cancels every resting order and pays one USDC per winning share.
    ResolveMarket {
        market: MarketId,
        winner: Outcome,
    },
    SetMarketInfo {
        market: MarketId,
        info: MarketInfo,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExchangeError {
    MarketExists,
    InvalidMarketConfig,
    InsufficientFunds,
    DepositLimit,
    UnknownMarket,
    InvalidTransition,
    InvalidMarketInfo,
}

/// Deterministic state machine: the same inputs with the same timestamps
/// always produce the same events and the same state.
#[derive(Default)]
pub struct Exchange {
    seq: u64,
    engine: Engine,
    ledger: Ledger,
    markets: BTreeMap<MarketId, MarketStatus>,
    info: BTreeMap<MarketId, MarketInfo>,
}

impl Exchange {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn market_status(&self, market: MarketId) -> Option<MarketStatus> {
        self.markets.get(&market).copied()
    }

    pub fn market_info(&self, market: MarketId) -> Option<&MarketInfo> {
        self.info.get(&market)
    }

    pub fn apply(&mut self, input: Input, now: u64) -> Result<Vec<Event>, ExchangeError> {
        use MarketStatus::{Closed, Open, Paused, Resolved};
        self.seq += 1;
        match input {
            Input::CreateMarket { market, config } => {
                if !config.is_valid() {
                    return Err(ExchangeError::InvalidMarketConfig);
                }
                if !self.engine.add_market(market, config) {
                    return Err(ExchangeError::MarketExists);
                }
                self.markets.insert(market, Open);
                Ok(Vec::new())
            }
            Input::Deposit { account, amount } => self
                .ledger
                .deposit(account, amount)
                .map(|()| Vec::new())
                .map_err(|_| ExchangeError::DepositLimit),
            Input::Withdraw { account, amount } => self
                .ledger
                .withdraw(account, amount)
                .map(|()| Vec::new())
                .map_err(|_| ExchangeError::InsufficientFunds),
            Input::Order(Command::Place(order)) => Ok(self.place(order, now)),
            Input::Order(cancel @ Command::Cancel { .. }) => {
                let events = self.engine.apply(cancel, now);
                self.settle(&events);
                Ok(events)
            }
            Input::Tick => {
                let events = self.engine.expire_all(now);
                self.settle(&events);
                Ok(events)
            }
            Input::SetMarketInfo { market, info } => {
                if !self.markets.contains_key(&market) {
                    return Err(ExchangeError::UnknownMarket);
                }
                if !info.is_valid() {
                    return Err(ExchangeError::InvalidMarketInfo);
                }
                self.info.insert(market, info);
                Ok(Vec::new())
            }
            Input::PauseMarket { market } => {
                self.transition(market, |s| matches!(s, Open), Paused)?;
                Ok(Vec::new())
            }
            Input::ResumeMarket { market } => {
                self.transition(market, |s| matches!(s, Paused), Open)?;
                Ok(Vec::new())
            }
            Input::CloseMarket { market } => {
                self.transition(market, |s| matches!(s, Open | Paused), Closed)?;
                Ok(self.cancel_all(market))
            }
            Input::ResolveMarket { market, winner } => {
                let unresolved = |s| !matches!(s, Resolved { .. });
                self.transition(market, unresolved, Resolved { winner })?;
                let events = self.cancel_all(market);
                self.ledger
                    .settle_market(market, winner)
                    .expect("every order of the market was just cancelled");
                Ok(events)
            }
        }
    }

    fn transition(
        &mut self,
        market: MarketId,
        allowed_from: impl Fn(MarketStatus) -> bool,
        to: MarketStatus,
    ) -> Result<(), ExchangeError> {
        let status = self
            .markets
            .get_mut(&market)
            .ok_or(ExchangeError::UnknownMarket)?;
        if !allowed_from(*status) {
            return Err(ExchangeError::InvalidTransition);
        }
        *status = to;
        Ok(())
    }

    fn cancel_all(&mut self, market: MarketId) -> Vec<Event> {
        let events = self.engine.cancel_all(market, CancelReason::MarketClosed);
        self.settle(&events);
        events
    }

    fn place(&mut self, order: NewOrder, now: u64) -> Vec<Event> {
        let reject = |reason| {
            vec![Event::Rejected {
                order_id: order.id,
                reason,
            }]
        };
        match self.market_status(order.market) {
            None => return reject(RejectReason::UnknownMarket),
            Some(MarketStatus::Open) => {}
            Some(_) => return reject(RejectReason::MarketNotOpen),
        }
        if let Err(e) = self.ledger.reserve(&order) {
            return reject(match e {
                LedgerError::InsufficientShares => RejectReason::InsufficientShares,
                LedgerError::DuplicateOrder => RejectReason::DuplicateOrderId,
                _ => RejectReason::InsufficientFunds,
            });
        }
        let events = self.engine.apply(Command::Place(order), now);
        self.settle(&events);
        events
    }

    fn settle(&mut self, events: &[Event]) {
        for event in events {
            self.ledger
                .apply(event)
                .expect("engine emitted an event for an order the ledger does not know");
        }
    }

    /// Canonical bytes: two exchanges in the same state encode identically.
    pub fn encode(&self) -> Vec<u8> {
        let state = (
            self.seq,
            self.engine.snapshot(),
            &self.ledger,
            &self.markets,
            &self.info,
        );
        postcard::to_allocvec(&state).expect("exchange state is serializable")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        type State = (
            u64,
            EngineSnapshot,
            Ledger,
            BTreeMap<MarketId, MarketStatus>,
            BTreeMap<MarketId, MarketInfo>,
        );
        let (seq, engine, ledger, markets, info): State = postcard::from_bytes(bytes)?;
        Ok(Exchange {
            seq,
            engine: Engine::restore(engine),
            ledger,
            markets,
            info,
        })
    }
}
