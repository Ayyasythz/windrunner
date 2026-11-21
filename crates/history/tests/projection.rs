use std::time::{SystemTime, UNIX_EPOCH};

use windrunner_engine::BookConfig;
use windrunner_exchange::{Applied, Exchange, Input};
use windrunner_history::History;
use windrunner_types::Outcome::{No, Yes};
use windrunner_types::Side::{Buy, Sell};
use windrunner_types::*;

const USDC: u64 = 1_000_000;
const SHARE: i64 = 1_000_000;
const MINUTE: u64 = 60_000;

/// These tests need a Postgres they may create schemas in, named by
/// WINDRUNNER_TEST_DATABASE_URL. Without it they pass without running.
async fn history() -> Option<History> {
    let url = std::env::var("WINDRUNNER_TEST_DATABASE_URL").ok()?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("t_{}_{nanos}", std::process::id());
    Some(History::connect_schema(&url, &schema).await.unwrap())
}

#[derive(Default)]
struct Recorder {
    exchange: Exchange,
    records: Vec<Applied>,
}

impl Recorder {
    fn seeded() -> Self {
        let mut recorder = Recorder::default();
        let config = BookConfig::default();
        recorder.apply(
            Input::CreateMarket {
                market: MarketId(1),
                config,
            },
            0,
        );
        for account in 1..=3 {
            let amount = 100 * USDC;
            recorder.apply(
                Input::Deposit {
                    account: AccountId(account),
                    amount,
                },
                0,
            );
        }
        recorder
    }

    fn apply(&mut self, input: Input, now: u64) -> u64 {
        let events = self.exchange.apply(input.clone(), now).unwrap_or_default();
        let seq = self.exchange.seq();
        self.records.push(Applied {
            seq,
            now,
            input,
            events,
        });
        seq
    }

    fn place(
        &mut self,
        account: u64,
        (side, outcome): (Side, Outcome),
        price: u16,
        shares: u64,
        tif: TimeInForce,
        now: u64,
    ) -> i64 {
        let id = self.exchange.seq() + 1;
        let order = NewOrder {
            id: OrderId(id),
            market: MarketId(1),
            account: AccountId(account),
            side,
            outcome,
            price: Price(price),
            qty: Qty(shares * SHARE as u64),
            tif,
            post_only: false,
        };
        self.apply(Input::Order(Command::Place(order)), now) as i64
    }
}

#[tokio::test]
async fn orders_and_trades_are_projected_once() {
    let Some(history) = history().await else {
        return;
    };
    let mut r = Recorder::seeded();
    let resting = r.place(1, (Buy, Yes), 6000, 10, TimeInForce::Gtc, 1_000);
    let taker = r.place(2, (Buy, No), 4000, 4, TimeInForce::Gtc, 2_000);
    let fak = r.place(2, (Buy, No), 4000, 10, TimeInForce::Fak, 3_000);
    let broke = r.place(3, (Buy, Yes), 5000, 900, TimeInForce::Gtc, 4_000);
    let cancel = Command::Cancel {
        market: MarketId(1),
        order_id: OrderId(taker as u64 + 10),
        account: AccountId(1),
    };
    r.apply(Input::Order(cancel), 5_000);

    let cursor = history.apply_batch(&r.records).await.unwrap();
    assert_eq!(cursor, r.exchange.seq());
    assert_eq!(history.cursor().await.unwrap(), cursor);
    assert_eq!(history.apply_batch(&r.records).await.unwrap(), cursor);

    let trades = history.market_trades(1, i64::MAX, 10).await.unwrap();
    assert_eq!(trades.len(), 2);
    assert_eq!(
        (trades[0].seq, trades[0].qty, trades[0].ts),
        (fak, 6 * SHARE, 3_000)
    );
    assert_eq!(
        (trades[1].seq, trades[1].qty, trades[1].yes_price),
        (taker, 4 * SHARE, 6000)
    );
    assert_eq!(trades[1].kind, "mint");
    assert_eq!(
        (
            trades[1].taker_side.as_str(),
            trades[1].taker_outcome.as_str()
        ),
        ("buy", "no")
    );
    let older = history.market_trades(1, trades[0].id, 10).await.unwrap();
    assert_eq!(older, trades[1..]);
    assert!(
        history
            .market_trades(2, i64::MAX, 10)
            .await
            .unwrap()
            .is_empty()
    );

    let alice = history.account_orders(1, None, i64::MAX, 10).await.unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(
        (alice[0].order_id, alice[0].status.as_str(), alice[0].filled),
        (resting, "filled", 10 * SHARE)
    );
    assert_eq!((alice[0].created_at, alice[0].updated_at), (1_000, 3_000));

    let bob = history.account_orders(2, None, i64::MAX, 10).await.unwrap();
    assert_eq!(bob.len(), 2);
    assert_eq!(
        (bob[0].order_id, bob[0].status.as_str(), bob[0].filled),
        (fak, "cancelled", 6 * SHARE)
    );
    assert_eq!(bob[0].reason.as_deref(), Some("unfilled"));
    assert_eq!((bob[0].tif.as_str(), bob[1].tif.as_str()), ("fak", "gtc"));
    assert_eq!((bob[1].order_id, bob[1].status.as_str()), (taker, "filled"));
    assert_eq!(
        history
            .account_orders(2, Some("cancelled"), i64::MAX, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        history
            .account_orders(2, None, fak, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    let carol = history.account_orders(3, None, i64::MAX, 10).await.unwrap();
    assert_eq!(
        (carol[0].order_id, carol[0].status.as_str()),
        (broke, "rejected")
    );
    assert_eq!(carol[0].reason.as_deref(), Some("insufficient_funds"));

    let fills = history.account_fills(1, i64::MAX, 10).await.unwrap();
    assert_eq!(fills.len(), 2);
    assert!(
        fills
            .iter()
            .all(|f| f.role == "maker" && f.order_id == resting && f.outcome == "yes")
    );
    let fills = history.account_fills(2, i64::MAX, 10).await.unwrap();
    assert_eq!(
        fills
            .iter()
            .map(|f| (f.order_id, f.role.as_str()))
            .collect::<Vec<_>>(),
        [(fak, "taker"), (taker, "taker")]
    );
}

#[tokio::test]
async fn cancelled_and_expired_orders_get_a_final_status() {
    let Some(history) = history().await else {
        return;
    };
    let mut r = Recorder::seeded();
    let cancelled = r.place(1, (Buy, Yes), 4000, 5, TimeInForce::Gtc, 1_000);
    let expiring = r.place(
        1,
        (Buy, Yes),
        3000,
        5,
        TimeInForce::Gtd { expires_at: 5_000 },
        1_000,
    );
    let closed = r.place(2, (Buy, No), 2000, 5, TimeInForce::Gtc, 1_000);
    let cancel = Command::Cancel {
        market: MarketId(1),
        order_id: OrderId(cancelled as u64),
        account: AccountId(1),
    };
    r.apply(Input::Order(cancel), 2_000);
    r.apply(Input::Tick, 5_000);
    r.apply(
        Input::ResolveMarket {
            market: MarketId(1),
            winner: Yes,
        },
        6_000,
    );
    history.apply_batch(&r.records).await.unwrap();

    let alice = history.account_orders(1, None, i64::MAX, 10).await.unwrap();
    assert_eq!(
        (
            alice[0].order_id,
            alice[0].reason.as_deref(),
            alice[0].expires_at
        ),
        (expiring, Some("expired"), Some(5_000))
    );
    assert_eq!(
        (
            alice[1].order_id,
            alice[1].reason.as_deref(),
            alice[1].updated_at
        ),
        (cancelled, Some("user"), 2_000)
    );
    let bob = history
        .account_orders(2, Some("cancelled"), i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(
        (bob[0].order_id, bob[0].reason.as_deref()),
        (closed, Some("market_closed"))
    );
    assert!(
        history
            .account_orders(1, Some("open"), i64::MAX, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn candles_aggregate_per_interval() {
    let Some(history) = history().await else {
        return;
    };
    let mut r = Recorder::seeded();
    let mut trade = |price: u16, shares: u64, now: u64| {
        r.place(1, (Buy, Yes), price, shares, TimeInForce::Gtc, now);
        r.place(2, (Buy, No), 10_000 - price, shares, TimeInForce::Gtc, now);
    };
    trade(5000, 1, 10 * MINUTE + 1_000);
    trade(5500, 2, 10 * MINUTE + 30_000);
    trade(4800, 3, 10 * MINUTE + 59_000);
    trade(5200, 4, 12 * MINUTE);
    trade(6000, 5, 16 * MINUTE + 5);
    history.apply_batch(&r.records).await.unwrap();

    let minute = history.candles(1, MINUTE as i64, 10).await.unwrap();
    assert_eq!(
        minute.iter().map(|c| c.start).collect::<Vec<_>>(),
        [16 * MINUTE as i64, 12 * MINUTE as i64, 10 * MINUTE as i64]
    );
    let first = &minute[2];
    assert_eq!(
        (first.open, first.high, first.low, first.close),
        (5000, 5500, 4800, 4800)
    );
    assert_eq!((first.volume, first.trades), (6 * SHARE, 3));

    let five = history.candles(1, 5 * MINUTE as i64, 10).await.unwrap();
    assert_eq!(five.len(), 2);
    assert_eq!(five[0].start, 15 * MINUTE as i64);
    let merged = &five[1];
    assert_eq!(merged.start, 10 * MINUTE as i64);
    assert_eq!(
        (merged.open, merged.high, merged.low, merged.close),
        (5000, 5500, 4800, 5200)
    );
    assert_eq!((merged.volume, merged.trades), (10 * SHARE, 4));
    assert_eq!(history.candles(1, MINUTE as i64, 1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn sell_side_and_gaps() {
    let Some(history) = history().await else {
        return;
    };
    let mut r = Recorder::seeded();
    r.place(1, (Buy, Yes), 6000, 10, TimeInForce::Gtc, 1_000);
    r.place(2, (Buy, No), 4000, 10, TimeInForce::Gtc, 1_000);
    let ask = r.place(1, (Sell, Yes), 7000, 10, TimeInForce::Gtc, 2_000);
    r.place(3, (Buy, Yes), 7000, 3, TimeInForce::Gtc, 3_000);

    // Attaching late: everything before the first record seen is simply absent.
    let late = &r.records[r.records.len() - 2..];
    assert_eq!(history.apply_batch(late).await.unwrap(), r.exchange.seq());
    let trades = history.market_trades(1, i64::MAX, 10).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(
        (trades[0].kind.as_str(), trades[0].yes_price, trades[0].qty),
        ("transfer_yes", 7000, 3 * SHARE)
    );
    let alice = history.account_orders(1, None, i64::MAX, 10).await.unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(
        (alice[0].order_id, alice[0].status.as_str(), alice[0].filled),
        (ask, "open", 3 * SHARE)
    );
    assert_eq!(alice[0].side, "sell");
}
