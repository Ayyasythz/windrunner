use proptest::prelude::*;
use windrunner_engine::{BookConfig, OrderBook, StpMode};
use windrunner_ledger::{Balance, Ledger, LedgerError};
use windrunner_types::Outcome::{No, Yes};
use windrunner_types::Side::{Buy, Sell};
use windrunner_types::*;

const USDC: u64 = 1_000_000;
const SHARE: u64 = QTY_SCALE;
const M: MarketId = MarketId(1);

struct Exchange {
    book: OrderBook,
    ledger: Ledger,
    now: u64,
}

impl Exchange {
    fn new(cfg: BookConfig) -> Self {
        Exchange {
            book: OrderBook::new(M, cfg),
            ledger: Ledger::new(),
            now: 0,
        }
    }

    fn funded(accounts: u64, usdc: u64) -> Self {
        let mut ex = Exchange::new(BookConfig::default());
        for a in 1..=accounts {
            ex.ledger.deposit(AccountId(a), usdc * USDC).unwrap();
        }
        ex
    }

    fn place(&mut self, o: NewOrder) -> Result<Vec<Event>, LedgerError> {
        self.ledger.reserve(&o)?;
        let events = self.book.place(o, self.now);
        self.settle(&events);
        Ok(events)
    }

    fn cancel(&mut self, id: u64, account: u64) {
        let events = self.book.cancel(OrderId(id), AccountId(account));
        self.settle(&events);
    }

    fn settle(&mut self, events: &[Event]) {
        for e in events {
            self.ledger.apply(e).unwrap();
        }
        self.book.assert_consistent();
        self.ledger.assert_consistent();
    }

    fn usdc(&self, account: u64) -> Balance {
        self.ledger.usdc(AccountId(account))
    }

    fn shares(&self, account: u64, outcome: Outcome) -> Balance {
        self.ledger.position(AccountId(account), M, outcome)
    }

    /// Account 1 ends up with 10 YES for 6 USDC, account 2 with 10 NO for 4 USDC.
    fn mint_ten(&mut self) {
        self.place(order(101, 1, Buy, Yes, 6000, 10)).unwrap();
        self.place(order(102, 2, Buy, No, 4000, 10)).unwrap();
    }
}

fn order(id: u64, account: u64, side: Side, outcome: Outcome, price: u16, shares: u64) -> NewOrder {
    NewOrder {
        id: OrderId(id),
        market: M,
        account: AccountId(account),
        side,
        outcome,
        price: Price(price),
        qty: Qty(shares * SHARE),
        tif: TimeInForce::Gtc,
        post_only: false,
    }
}

fn free(amount: u64) -> Balance {
    Balance {
        available: amount,
        reserved: 0,
    }
}

#[test]
fn orders_need_funds_or_shares() {
    let mut ex = Exchange::funded(1, 5);
    assert_eq!(
        ex.place(order(1, 1, Buy, Yes, 6000, 10)).unwrap_err(),
        LedgerError::InsufficientFunds
    );
    assert_eq!(
        ex.place(order(2, 1, Sell, Yes, 6000, 1)).unwrap_err(),
        LedgerError::InsufficientShares
    );
    assert_eq!(
        ex.place(order(3, 9, Buy, Yes, 6000, 1)).unwrap_err(),
        LedgerError::InsufficientFunds
    );
    assert_eq!(ex.usdc(1), free(5 * USDC));
    assert!(ex.book.is_empty());
}

#[test]
fn resting_buy_reserves_and_cancel_releases() {
    let mut ex = Exchange::funded(1, 100);
    ex.place(order(1, 1, Buy, Yes, 4000, 10)).unwrap();
    assert_eq!(
        ex.usdc(1),
        Balance {
            available: 96 * USDC,
            reserved: 4 * USDC
        }
    );
    assert_eq!(
        ex.ledger.withdraw(AccountId(1), 97 * USDC),
        Err(LedgerError::InsufficientFunds)
    );
    assert_eq!(
        ex.place(order(1, 1, Buy, Yes, 4000, 10)).unwrap_err(),
        LedgerError::DuplicateOrder
    );

    ex.cancel(1, 1);
    assert_eq!(ex.usdc(1), free(100 * USDC));
    assert_eq!(ex.ledger.withdraw(AccountId(1), 100 * USDC), Ok(()));
    ex.ledger.assert_consistent();
}

#[test]
fn mint_locks_one_usdc_per_share() {
    let mut ex = Exchange::funded(2, 100);
    ex.mint_ten();
    assert_eq!(ex.usdc(1), free(94 * USDC));
    assert_eq!(ex.usdc(2), free(96 * USDC));
    assert_eq!(ex.shares(1, Yes), free(10 * SHARE));
    assert_eq!(ex.shares(2, No), free(10 * SHARE));
    assert_eq!(ex.ledger.collateral(M), 10 * USDC);
}

#[test]
fn taker_gets_price_improvement_refunded() {
    let mut ex = Exchange::funded(3, 100);
    ex.mint_ten();
    ex.place(order(1, 1, Sell, Yes, 5000, 10)).unwrap();
    assert_eq!(
        ex.shares(1, Yes),
        Balance {
            available: 0,
            reserved: 10 * SHARE
        }
    );

    ex.place(order(2, 3, Buy, Yes, 7000, 10)).unwrap();
    assert_eq!(ex.usdc(3), free(95 * USDC));
    assert_eq!(ex.shares(3, Yes), free(10 * SHARE));
    assert_eq!(ex.usdc(1), free(99 * USDC));
    assert_eq!(ex.shares(1, Yes), free(0));
    assert_eq!(ex.ledger.collateral(M), 10 * USDC);
}

#[test]
fn merge_returns_collateral() {
    let mut ex = Exchange::funded(2, 100);
    ex.mint_ten();
    ex.place(order(1, 1, Sell, Yes, 5500, 10)).unwrap();
    ex.place(order(2, 2, Sell, No, 4500, 10)).unwrap();
    assert_eq!(ex.usdc(1), free(99_500_000));
    assert_eq!(ex.usdc(2), free(100_500_000));
    assert_eq!(ex.shares(1, Yes), free(0));
    assert_eq!(ex.shares(2, No), free(0));
    assert_eq!(ex.ledger.collateral(M), 0);
}

#[test]
fn engine_rejection_releases_reservation() {
    let mut ex = Exchange::funded(2, 100);
    ex.place(order(1, 1, Sell, No, 4000, 1)).unwrap_err();
    ex.place(order(2, 1, Buy, Yes, 5000, 10)).unwrap();

    let fok = NewOrder {
        tif: TimeInForce::Fok,
        ..order(3, 2, Buy, No, 5000, 20)
    };
    let events = ex.place(fok).unwrap();
    assert!(matches!(events[0], Event::Rejected { .. }));
    assert_eq!(ex.usdc(2), free(100 * USDC));

    let fak = NewOrder {
        tif: TimeInForce::Fak,
        ..order(4, 2, Buy, No, 5000, 20)
    };
    ex.place(fak).unwrap();
    assert_eq!(ex.usdc(2), free(95 * USDC));
    assert_eq!(ex.shares(2, No), free(10 * SHARE));
}

#[test]
fn settlement_pays_winners() {
    let mut ex = Exchange::funded(2, 100);
    ex.mint_ten();
    ex.place(order(1, 1, Sell, Yes, 9000, 5)).unwrap();
    assert_eq!(
        ex.ledger.settle_market(M, Yes),
        Err(LedgerError::OpenOrdersRemain)
    );
    ex.cancel(1, 1);

    assert_eq!(ex.ledger.settle_market(M, Yes), Ok(10 * USDC));
    assert_eq!(ex.usdc(1), free(104 * USDC));
    assert_eq!(ex.usdc(2), free(96 * USDC));
    assert_eq!(ex.shares(2, No), free(0));
    assert_eq!(ex.ledger.collateral(M), 0);
    ex.ledger.assert_consistent();
}

#[derive(Clone, Debug)]
enum Op {
    Place {
        account: u64,
        side: Side,
        outcome: Outcome,
        tick: u16,
        lots: u64,
        tif: u8,
        ttl: u64,
    },
    Cancel {
        pick: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let place = (
        1u64..5,
        prop_oneof![Just(Buy), Just(Sell)],
        prop_oneof![Just(Yes), Just(No)],
        35u16..66,
        1u64..400,
        0u8..6,
        1u64..20,
    )
        .prop_map(|(account, side, outcome, tick, lots, tif, ttl)| Op::Place {
            account,
            side,
            outcome,
            tick,
            lots,
            tif,
            ttl,
        });
    let cancel = any::<usize>().prop_map(|pick| Op::Cancel { pick });
    prop_oneof![5 => place, 1 => cancel]
}

proptest! {
    #[test]
    fn money_and_shares_are_conserved(
        ops in prop::collection::vec(op(), 1..300),
        cancel_maker in any::<bool>(),
        yes_wins in any::<bool>(),
    ) {
        const ACCOUNTS: u64 = 4;
        const FUNDS: u64 = 20 * USDC;
        let cfg = BookConfig {
            stp: if cancel_maker { StpMode::CancelMaker } else { StpMode::CancelTaker },
            ..BookConfig::default()
        };
        let mut ex = Exchange::new(cfg);
        for a in 1..=ACCOUNTS {
            ex.ledger.deposit(AccountId(a), FUNDS).unwrap();
        }
        let mut placed: Vec<(u64, u64)> = Vec::new();
        let mut next_id = 1;

        for (now, op) in ops.into_iter().enumerate() {
            ex.now = now as u64;
            match op {
                Op::Place { account, side, outcome, tick, lots, tif, ttl } => {
                    let tif = match tif {
                        0 => TimeInForce::Fok,
                        1 => TimeInForce::Fak,
                        2 => TimeInForce::Gtd { expires_at: ex.now + ttl },
                        _ => TimeInForce::Gtc,
                    };
                    let o = NewOrder {
                        qty: Qty(lots * cfg.lot),
                        tif,
                        ..order(next_id, account, side, outcome, tick * cfg.tick, 0)
                    };
                    next_id += 1;
                    if ex.place(o).is_ok() {
                        placed.push((o.id.0, account));
                    }
                }
                Op::Cancel { pick } => {
                    if let Some(&(id, account)) = placed.get(pick % placed.len().max(1)) {
                        ex.cancel(id, account);
                    }
                }
            }
        }

        for (id, account) in placed {
            ex.cancel(id, account);
        }
        prop_assert!(ex.book.is_empty());
        let winner = if yes_wins { Yes } else { No };
        ex.ledger.settle_market(M, winner).unwrap();
        ex.ledger.assert_consistent();

        let mut total = 0;
        for a in 1..=ACCOUNTS {
            let balance = ex.usdc(a);
            prop_assert_eq!(balance.reserved, 0);
            prop_assert_eq!(ex.shares(a, Yes), free(0));
            prop_assert_eq!(ex.shares(a, No), free(0));
            total += balance.available;
        }
        prop_assert_eq!(total, ACCOUNTS * FUNDS);
    }
}
