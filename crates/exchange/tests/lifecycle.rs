use windrunner_engine::BookConfig;
use windrunner_exchange::{Exchange, ExchangeError, Input};
use windrunner_types::Outcome::{No, Yes};
use windrunner_types::Side::{Buy, Sell};
use windrunner_types::*;

const USDC: u64 = 1_000_000;
const SHARE: u64 = QTY_SCALE;
const M: MarketId = MarketId(1);

fn exchange() -> Exchange {
    let mut ex = Exchange::new();
    let config = BookConfig::default();
    ex.apply(Input::CreateMarket { market: M, config }, 0)
        .unwrap();
    for account in 1..=2 {
        let deposit = Input::Deposit {
            account: AccountId(account),
            amount: 100 * USDC,
        };
        ex.apply(deposit, 0).unwrap();
    }
    ex
}

fn place(
    ex: &mut Exchange,
    id: u64,
    account: u64,
    side: Side,
    outcome: Outcome,
    price: u16,
    shares: u64,
) -> Vec<Event> {
    let order = NewOrder {
        id: OrderId(id),
        market: M,
        account: AccountId(account),
        side,
        outcome,
        price: Price(price),
        qty: Qty(shares * SHARE),
        tif: TimeInForce::Gtc,
        post_only: false,
    };
    ex.apply(Input::Order(Command::Place(order)), 0).unwrap()
}

fn rejected(events: &[Event]) -> Option<RejectReason> {
    match events {
        [Event::Rejected { reason, .. }] => Some(*reason),
        _ => None,
    }
}

fn available(ex: &Exchange, account: u64) -> u64 {
    let balance = ex.ledger().usdc(AccountId(account));
    assert_eq!(balance.reserved, 0);
    balance.available
}

#[test]
fn paused_market_refuses_orders_but_allows_cancels() {
    let mut ex = exchange();
    place(&mut ex, 1, 1, Buy, Yes, 6000, 10);
    ex.apply(Input::PauseMarket { market: M }, 0).unwrap();
    assert_eq!(ex.market_status(M), Some(MarketStatus::Paused));
    assert_eq!(
        ex.apply(Input::PauseMarket { market: M }, 0),
        Err(ExchangeError::InvalidTransition)
    );

    let events = place(&mut ex, 2, 2, Buy, No, 4000, 10);
    assert_eq!(rejected(&events), Some(RejectReason::MarketNotOpen));
    assert_eq!(available(&ex, 2), 100 * USDC);

    let cancel = Command::Cancel {
        market: M,
        order_id: OrderId(1),
        account: AccountId(1),
    };
    let events = ex.apply(Input::Order(cancel), 0).unwrap();
    assert!(matches!(events[..], [Event::Cancelled { .. }]));

    ex.apply(Input::ResumeMarket { market: M }, 0).unwrap();
    assert_eq!(rejected(&place(&mut ex, 3, 2, Buy, No, 4000, 10)), None);
    ex.ledger().assert_consistent();
}

#[test]
fn closing_cancels_everything_and_is_final() {
    let mut ex = exchange();
    place(&mut ex, 1, 1, Buy, Yes, 6000, 10);
    place(&mut ex, 2, 2, Buy, No, 3000, 10);

    let events = ex.apply(Input::CloseMarket { market: M }, 0).unwrap();
    let cancelled: Vec<u64> = events
        .iter()
        .map(|e| match e {
            Event::Cancelled {
                order_id,
                reason: CancelReason::MarketClosed,
                ..
            } => order_id.0,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(cancelled, vec![1, 2]);
    assert!(ex.engine().book(M).unwrap().is_empty());
    assert_eq!(available(&ex, 1), 100 * USDC);
    assert_eq!(available(&ex, 2), 100 * USDC);

    for input in [
        Input::ResumeMarket { market: M },
        Input::PauseMarket { market: M },
        Input::CloseMarket { market: M },
    ] {
        assert_eq!(ex.apply(input, 0), Err(ExchangeError::InvalidTransition));
    }
    let events = place(&mut ex, 3, 1, Buy, Yes, 6000, 1);
    assert_eq!(rejected(&events), Some(RejectReason::MarketNotOpen));
}

#[test]
fn resolving_pays_the_winning_side() {
    for (winner, alice, bob) in [(Yes, 104, 96), (No, 94, 106)] {
        let mut ex = exchange();
        place(&mut ex, 1, 1, Buy, Yes, 6000, 10);
        place(&mut ex, 2, 2, Buy, No, 4000, 10);
        place(&mut ex, 3, 1, Sell, Yes, 9000, 5);
        place(&mut ex, 4, 2, Buy, Yes, 1000, 20);

        let events = ex
            .apply(Input::ResolveMarket { market: M, winner }, 0)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(ex.market_status(M), Some(MarketStatus::Resolved { winner }));
        assert_eq!(available(&ex, 1), alice * USDC);
        assert_eq!(available(&ex, 2), bob * USDC);
        assert_eq!(ex.ledger().collateral(M), 0);
        assert!(ex.ledger().positions_of(AccountId(1)).is_empty());
        ex.ledger().assert_consistent();

        assert_eq!(
            ex.apply(Input::ResolveMarket { market: M, winner }, 0),
            Err(ExchangeError::InvalidTransition)
        );
        let events = place(&mut ex, 5, 1, Buy, Yes, 6000, 1);
        assert_eq!(rejected(&events), Some(RejectReason::MarketNotOpen));
    }
}

#[test]
fn lifecycle_of_unknown_market_fails() {
    let mut ex = exchange();
    let market = MarketId(9);
    for input in [
        Input::PauseMarket { market },
        Input::ResumeMarket { market },
        Input::CloseMarket { market },
        Input::ResolveMarket {
            market,
            winner: Yes,
        },
    ] {
        assert_eq!(ex.apply(input, 0), Err(ExchangeError::UnknownMarket));
    }
}

#[test]
fn status_survives_snapshot() {
    let mut ex = exchange();
    place(&mut ex, 1, 1, Buy, Yes, 6000, 10);
    ex.apply(Input::PauseMarket { market: M }, 0).unwrap();
    let info = MarketInfo {
        title: "Will it rain?".into(),
        description: String::new(),
        closes_at: Some(42),
    };
    let set = Input::SetMarketInfo {
        market: M,
        info: info.clone(),
    };
    ex.apply(set, 0).unwrap();
    let restored = Exchange::decode(&ex.encode()).unwrap();
    assert_eq!(restored.market_status(M), Some(MarketStatus::Paused));
    assert_eq!(restored.market_info(M), Some(&info));
    assert_eq!(restored.encode(), ex.encode());
}
