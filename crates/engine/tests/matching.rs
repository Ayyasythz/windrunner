use windrunner_engine::{BookConfig, Engine, OrderBook, StpMode};
use windrunner_types::Outcome::{No, Yes};
use windrunner_types::Side::{Buy, Sell};
use windrunner_types::*;

const SHARE: u64 = QTY_SCALE;

fn book() -> OrderBook {
    OrderBook::new(MarketId(1), BookConfig::default())
}

fn order(id: u64, account: u64, side: Side, outcome: Outcome, price: u16, shares: u64) -> NewOrder {
    NewOrder {
        id: OrderId(id),
        market: MarketId(1),
        account: AccountId(account),
        side,
        outcome,
        price: Price(price),
        qty: Qty(shares * SHARE),
        tif: TimeInForce::Gtc,
        post_only: false,
    }
}

fn trades(events: &[Event]) -> Vec<Trade> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Trade(t) => Some(*t),
            _ => None,
        })
        .collect()
}

fn rejected(events: &[Event]) -> Option<RejectReason> {
    events.iter().find_map(|e| match e {
        Event::Rejected { reason, .. } | Event::CancelRejected { reason, .. } => Some(*reason),
        _ => None,
    })
}

#[test]
fn rests_when_nothing_crosses() {
    let mut b = book();
    let ev = b.place(order(1, 1, Buy, Yes, 4000, 10), 0);
    assert_eq!(
        ev,
        vec![
            Event::Accepted {
                order_id: OrderId(1)
            },
            Event::Rested {
                order_id: OrderId(1),
                remaining: Qty(10 * SHARE)
            },
        ]
    );
    b.place(order(2, 2, Sell, Yes, 6000, 5), 0);
    assert_eq!(b.best_bid(), Some(Price(4000)));
    assert_eq!(b.best_ask(), Some(Price(6000)));
    b.assert_consistent();
}

#[test]
fn transfer_yes_executes_at_maker_price() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 6000, 10), 0);
    let t = trades(&b.place(order(2, 2, Buy, Yes, 6500, 10), 0));
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].kind, SettlementKind::TransferYes);
    assert_eq!(t[0].yes_price, Price(6000));
    assert_eq!(t[0].qty, Qty(10 * SHARE));
    assert!(b.is_empty());
}

#[test]
fn buy_yes_and_buy_no_mint_when_prices_sum_to_one() {
    let mut b = book();
    b.place(order(1, 1, Buy, Yes, 6000, 10), 0);
    let t = trades(&b.place(order(2, 2, Buy, No, 4000, 10), 0));
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].kind, SettlementKind::Mint);
    assert_eq!(t[0].price_for(Yes), Price(6000));
    assert_eq!(t[0].price_for(No), Price(4000));
    assert_eq!(
        notional(t[0].price_for(Yes), t[0].qty) + notional(t[0].price_for(No), t[0].qty),
        10 * SHARE
    );
}

#[test]
fn buy_yes_and_buy_no_rest_when_prices_sum_below_one() {
    let mut b = book();
    b.place(order(1, 1, Buy, Yes, 6000, 10), 0);
    let ev = b.place(order(2, 2, Buy, No, 3900, 10), 0);
    assert!(trades(&ev).is_empty());
    assert_eq!(b.best_bid(), Some(Price(6000)));
    assert_eq!(b.best_ask(), Some(Price(6100)));
}

#[test]
fn sell_yes_and_sell_no_merge() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 5500, 10), 0);
    let t = trades(&b.place(order(2, 2, Sell, No, 4500, 4), 0));
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].kind, SettlementKind::Merge);
    assert_eq!(t[0].yes_price, Price(5500));
    assert_eq!(t[0].maker.remaining, Qty(6 * SHARE));
    assert_eq!(b.remaining(OrderId(1)), Some(Qty(6 * SHARE)));
}

#[test]
fn transfer_no() {
    let mut b = book();
    b.place(order(1, 1, Buy, No, 3000, 10), 0);
    let t = trades(&b.place(order(2, 2, Sell, No, 3000, 10), 0));
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].kind, SettlementKind::TransferNo);
    assert_eq!(t[0].price_for(No), Price(3000));
}

#[test]
fn price_then_time_priority() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 6000, 5), 0);
    b.place(order(2, 2, Sell, Yes, 5900, 5), 0);
    b.place(order(3, 3, Sell, Yes, 5900, 5), 0);
    b.place(order(4, 4, Buy, No, 4100, 5), 0);

    let t = trades(&b.place(order(9, 9, Buy, Yes, 6000, 18), 0));
    let makers: Vec<u64> = t.iter().map(|t| t.maker.order_id.0).collect();
    assert_eq!(makers, vec![2, 3, 4, 1]);
    assert_eq!(t[3].qty, Qty(3 * SHARE));
    assert_eq!(b.remaining(OrderId(1)), Some(Qty(2 * SHARE)));
    b.assert_consistent();
}

#[test]
fn partial_fill_rests_remainder() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 5000, 3), 0);
    let ev = b.place(order(2, 2, Buy, Yes, 5000, 10), 0);
    assert_eq!(
        ev.last(),
        Some(&Event::Rested {
            order_id: OrderId(2),
            remaining: Qty(7 * SHARE)
        })
    );
    assert_eq!(
        b.depth(BookSide::Bid, 5),
        vec![(Price(5000), Qty(7 * SHARE))]
    );
    assert_eq!(b.best_ask(), None);
}

#[test]
fn fak_cancels_remainder() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 5000, 3), 0);
    let ev = b.place(
        NewOrder {
            tif: TimeInForce::Fak,
            ..order(2, 2, Buy, Yes, 5000, 10)
        },
        0,
    );
    assert_eq!(trades(&ev).len(), 1);
    assert_eq!(
        ev.last(),
        Some(&Event::Cancelled {
            market: MarketId(1),
            order_id: OrderId(2),
            account: AccountId(2),
            remaining: Qty(7 * SHARE),
            reason: CancelReason::Unfilled,
        })
    );
    assert!(b.is_empty());
}

#[test]
fn fok_is_all_or_nothing() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 5000, 3), 0);
    b.place(order(2, 2, Sell, Yes, 5100, 3), 0);

    let fok = |id, price, shares| NewOrder {
        tif: TimeInForce::Fok,
        ..order(id, 9, Buy, Yes, price, shares)
    };
    assert_eq!(
        rejected(&b.place(fok(3, 5000, 5), 0)),
        Some(RejectReason::FokNotFillable)
    );
    assert_eq!(b.len(), 2);

    let t = trades(&b.place(fok(4, 5100, 5), 0));
    assert_eq!(t.iter().map(|t| t.qty.0).sum::<u64>(), 5 * SHARE);
    assert_eq!(b.remaining(OrderId(2)), Some(Qty(SHARE)));
}

#[test]
fn post_only_rejects_when_crossing() {
    let mut b = book();
    b.place(order(1, 1, Sell, Yes, 5000, 3), 0);
    let po = |id, price| NewOrder {
        post_only: true,
        ..order(id, 2, Buy, Yes, price, 1)
    };
    assert_eq!(
        rejected(&b.place(po(2, 5000), 0)),
        Some(RejectReason::PostOnlyWouldCross)
    );
    assert_eq!(rejected(&b.place(po(3, 4900), 0)), None);
    assert_eq!(b.best_bid(), Some(Price(4900)));
}

#[test]
fn cancel_checks_ownership() {
    let mut b = book();
    b.place(order(1, 1, Buy, Yes, 4000, 10), 0);
    assert_eq!(
        rejected(&b.cancel(OrderId(1), AccountId(2))),
        Some(RejectReason::NotOwner)
    );
    assert_eq!(
        rejected(&b.cancel(OrderId(7), AccountId(1))),
        Some(RejectReason::UnknownOrder)
    );
    assert_eq!(
        b.cancel(OrderId(1), AccountId(1)),
        vec![Event::Cancelled {
            market: MarketId(1),
            order_id: OrderId(1),
            account: AccountId(1),
            remaining: Qty(10 * SHARE),
            reason: CancelReason::User,
        }]
    );
    assert_eq!(b.best_bid(), None);
    b.assert_consistent();
}

#[test]
fn self_trade_cancels_taker_by_default() {
    let mut b = book();
    b.place(order(1, 2, Sell, Yes, 4900, 2), 0);
    b.place(order(2, 1, Sell, Yes, 5000, 5), 0);
    let ev = b.place(order(3, 1, Buy, Yes, 5000, 5), 0);
    assert_eq!(trades(&ev).len(), 1);
    assert_eq!(
        ev.last(),
        Some(&Event::Cancelled {
            market: MarketId(1),
            order_id: OrderId(3),
            account: AccountId(1),
            remaining: Qty(3 * SHARE),
            reason: CancelReason::SelfTrade,
        })
    );
    assert_eq!(b.remaining(OrderId(2)), Some(Qty(5 * SHARE)));
}

#[test]
fn self_trade_can_cancel_maker_instead() {
    let cfg = BookConfig {
        stp: StpMode::CancelMaker,
        ..BookConfig::default()
    };
    let mut b = OrderBook::new(MarketId(1), cfg);
    b.place(order(1, 1, Sell, Yes, 5000, 5), 0);
    b.place(order(2, 2, Sell, Yes, 5000, 5), 0);
    let ev = b.place(order(3, 1, Buy, Yes, 5000, 5), 0);
    assert!(ev.contains(&Event::Cancelled {
        market: MarketId(1),
        order_id: OrderId(1),
        account: AccountId(1),
        remaining: Qty(5 * SHARE),
        reason: CancelReason::SelfTrade,
    }));
    let t = trades(&ev);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].maker.order_id, OrderId(2));
    assert!(b.is_empty());
}

#[test]
fn gtd_orders_expire() {
    let mut b = book();
    let gtd = NewOrder {
        tif: TimeInForce::Gtd { expires_at: 100 },
        ..order(1, 1, Sell, Yes, 5000, 5)
    };
    b.place(gtd, 10);
    assert_eq!(
        rejected(&b.place(
            NewOrder {
                id: OrderId(2),
                ..gtd
            },
            100
        )),
        Some(RejectReason::AlreadyExpired)
    );

    let mut b = book();
    b.place(gtd, 10);
    let ev = b.place(order(3, 2, Buy, Yes, 5000, 5), 100);
    assert!(trades(&ev).is_empty());
    assert_eq!(
        ev[0],
        Event::Cancelled {
            market: MarketId(1),
            order_id: OrderId(1),
            account: AccountId(1),
            remaining: Qty(5 * SHARE),
            reason: CancelReason::Expired,
        }
    );
    assert_eq!(b.best_bid(), Some(Price(5000)));
    assert_eq!(b.best_ask(), None);
}

#[test]
fn validation() {
    let mut b = book();
    let base = order(1, 1, Buy, Yes, 5000, 1);
    let reject = |b: &mut OrderBook, o| rejected(&b.place(o, 0));

    assert_eq!(
        reject(
            &mut b,
            NewOrder {
                price: Price(5050),
                ..base
            }
        ),
        Some(RejectReason::InvalidPrice)
    );
    assert_eq!(
        reject(
            &mut b,
            NewOrder {
                price: Price(0),
                ..base
            }
        ),
        Some(RejectReason::InvalidPrice)
    );
    assert_eq!(
        reject(
            &mut b,
            NewOrder {
                price: Price(10_000),
                ..base
            }
        ),
        Some(RejectReason::InvalidPrice)
    );
    assert_eq!(
        reject(
            &mut b,
            NewOrder {
                qty: Qty(0),
                ..base
            }
        ),
        Some(RejectReason::InvalidQty)
    );
    assert_eq!(
        reject(
            &mut b,
            NewOrder {
                qty: Qty(15_000),
                ..base
            }
        ),
        Some(RejectReason::InvalidQty)
    );
    assert_eq!(
        reject(
            &mut b,
            NewOrder {
                tif: TimeInForce::Fak,
                post_only: true,
                ..base
            }
        ),
        Some(RejectReason::InvalidTimeInForce)
    );
    assert_eq!(reject(&mut b, base), None);
    assert_eq!(reject(&mut b, base), Some(RejectReason::DuplicateOrderId));
}

#[test]
fn engine_routes_by_market() {
    let mut e = Engine::new();
    assert!(e.add_market(MarketId(1), BookConfig::default()));
    assert!(!e.add_market(MarketId(1), BookConfig::default()));

    let o = order(1, 1, Buy, Yes, 5000, 1);
    assert_eq!(
        rejected(&e.apply(
            Command::Place(NewOrder {
                market: MarketId(2),
                ..o
            }),
            0
        )),
        Some(RejectReason::UnknownMarket)
    );
    e.apply(Command::Place(o), 0);
    assert_eq!(e.book(MarketId(1)).unwrap().best_bid(), Some(Price(5000)));

    let cancel = Command::Cancel {
        market: MarketId(1),
        order_id: OrderId(1),
        account: AccountId(1),
    };
    e.apply(cancel, 0);
    assert!(e.book(MarketId(1)).unwrap().is_empty());
}
