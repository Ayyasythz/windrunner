use std::collections::HashMap;

use proptest::prelude::*;
use windrunner_engine::{BookConfig, OrderBook, StpMode};
use windrunner_types::*;

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
        post_only: bool,
    },
    Cancel {
        pick: usize,
        account: u64,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let place = (
        1u64..5,
        prop_oneof![Just(Side::Buy), Just(Side::Sell)],
        prop_oneof![Just(Outcome::Yes), Just(Outcome::No)],
        35u16..66,
        1u64..50,
        0u8..6,
        1u64..20,
        prop::bool::weighted(0.1),
    )
        .prop_map(
            |(account, side, outcome, tick, lots, tif, ttl, post_only)| Op::Place {
                account,
                side,
                outcome,
                tick,
                lots,
                tif,
                ttl,
                post_only,
            },
        );
    let cancel = (any::<usize>(), 1u64..5).prop_map(|(pick, account)| Op::Cancel { pick, account });
    prop_oneof![4 => place, 1 => cancel]
}

#[derive(Default)]
struct Tally {
    qty: u64,
    filled: u64,
    cancelled: u64,
}

proptest! {
    #[test]
    fn book_stays_consistent_and_conserves_quantity(
        ops in prop::collection::vec(op(), 1..300),
        cancel_maker in any::<bool>(),
    ) {
        let cfg = BookConfig {
            stp: if cancel_maker { StpMode::CancelMaker } else { StpMode::CancelTaker },
            ..BookConfig::default()
        };
        let mut book = OrderBook::new(MarketId(1), cfg);
        let mut orders: HashMap<OrderId, Tally> = HashMap::new();
        let mut ids: Vec<OrderId> = Vec::new();

        for (now, op) in ops.into_iter().enumerate() {
            let now = now as u64;
            let events = match op {
                Op::Place { account, side, outcome, tick, lots, tif, ttl, post_only } => {
                    let id = OrderId(ids.len() as u64 + 1);
                    let tif = match tif {
                        0 => TimeInForce::Fok,
                        1 => TimeInForce::Fak,
                        2 => TimeInForce::Gtd { expires_at: now + ttl },
                        _ => TimeInForce::Gtc,
                    };
                    let post_only = post_only && !matches!(tif, TimeInForce::Fok | TimeInForce::Fak);
                    let qty = Qty(lots * cfg.lot);
                    ids.push(id);
                    orders.insert(id, Tally { qty: qty.0, ..Tally::default() });
                    book.place(
                        NewOrder {
                            id,
                            market: MarketId(1),
                            account: AccountId(account),
                            side,
                            outcome,
                            price: Price(tick * cfg.tick),
                            qty,
                            tif,
                            post_only,
                        },
                        now,
                    )
                }
                Op::Cancel { pick, account } => {
                    if ids.is_empty() {
                        continue;
                    }
                    book.cancel(ids[pick % ids.len()], AccountId(account))
                }
            };

            for e in events {
                match e {
                    Event::Trade(t) => {
                        prop_assert_ne!(t.maker.account, t.taker.account);
                        for fill in [t.maker, t.taker] {
                            let tally = orders.get_mut(&fill.order_id).unwrap();
                            tally.filled += t.qty.0;
                            prop_assert_eq!(tally.qty - tally.filled, fill.remaining.0);
                        }
                    }
                    Event::Cancelled { order_id, remaining, .. } => {
                        let tally = orders.get_mut(&order_id).unwrap();
                        prop_assert_eq!(tally.cancelled, 0);
                        tally.cancelled = remaining.0;
                    }
                    Event::Rejected { order_id, .. } => {
                        let tally = orders.get_mut(&order_id).unwrap();
                        tally.cancelled = tally.qty;
                    }
                    Event::Accepted { .. }
                    | Event::Rested { .. }
                    | Event::CancelRejected { .. } => {}
                }
            }
            book.assert_consistent();
        }

        for (id, tally) in &orders {
            let resting = book.remaining(*id).map_or(0, |q| q.0);
            prop_assert_eq!(tally.filled + tally.cancelled + resting, tally.qty, "order {:?}", id);
        }
    }
}
