use std::fs::{self, OpenOptions};
use std::path::Path;

use proptest::prelude::*;
use windrunner_engine::BookConfig;
use windrunner_exchange::{Exchange, ExchangeError, Input, Sequencer};
use windrunner_types::Outcome::{No, Yes};
use windrunner_types::Side::{Buy, Sell};
use windrunner_types::*;

const USDC: u64 = 1_000_000;
const SHARE: u64 = QTY_SCALE;

fn setup() -> Vec<Input> {
    let mut inputs = vec![
        Input::CreateMarket {
            market: MarketId(1),
            config: BookConfig::default(),
        },
        Input::CreateMarket {
            market: MarketId(2),
            config: BookConfig::default(),
        },
    ];
    inputs.extend((1..=4).map(|a| Input::Deposit {
        account: AccountId(a),
        amount: 20 * USDC,
    }));
    inputs
}

fn place(id: u64, account: u64, side: Side, outcome: Outcome, price: u16, qty: u64) -> Input {
    Input::Order(Command::Place(NewOrder {
        id: OrderId(id),
        market: MarketId(1),
        account: AccountId(account),
        side,
        outcome,
        price: Price(price),
        qty: Qty(qty),
        tif: TimeInForce::Gtc,
        post_only: false,
    }))
}

fn run(inputs: &[Input]) -> Exchange {
    let mut exchange = Exchange::new();
    for (now, input) in inputs.iter().enumerate() {
        let _ = exchange.apply(input.clone(), now as u64);
    }
    exchange
}

fn submit_all(sequencer: &mut Sequencer, inputs: &[Input]) {
    let start = sequencer.exchange().seq();
    for (i, input) in inputs.iter().enumerate() {
        let _ = sequencer.submit(input.clone(), start + i as u64).unwrap();
    }
}

fn wal_file(dir: &Path) -> std::path::PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "log"))
        .unwrap()
}

#[test]
fn exchange_rejects_what_the_ledger_cannot_cover() {
    let mut ex = run(&setup());
    let rejected = |ex: &mut Exchange, input| match ex.apply(input, 0).unwrap()[..] {
        [Event::Rejected { reason, .. }] => reason,
        ref other => panic!("expected a single rejection, got {other:?}"),
    };
    assert_eq!(
        rejected(&mut ex, place(1, 1, Buy, Yes, 5000, 100 * SHARE)),
        RejectReason::InsufficientFunds
    );
    assert_eq!(
        rejected(&mut ex, place(2, 1, Sell, Yes, 5000, SHARE)),
        RejectReason::InsufficientShares
    );
    assert_eq!(
        rejected(&mut ex, place(3, 9, Buy, Yes, 5000, SHARE)),
        RejectReason::InsufficientFunds
    );
    ex.apply(place(4, 1, Buy, Yes, 5000, SHARE), 0).unwrap();
    assert_eq!(
        rejected(&mut ex, place(4, 2, Buy, Yes, 5000, SHARE)),
        RejectReason::DuplicateOrderId
    );

    let create = Input::CreateMarket {
        market: MarketId(1),
        config: BookConfig::default(),
    };
    assert_eq!(ex.apply(create, 0), Err(ExchangeError::MarketExists));
    let bad = Input::CreateMarket {
        market: MarketId(7),
        config: BookConfig {
            tick: 3,
            ..BookConfig::default()
        },
    };
    assert_eq!(ex.apply(bad, 0), Err(ExchangeError::InvalidMarketConfig));
    let withdraw = Input::Withdraw {
        account: AccountId(1),
        amount: 20 * USDC,
    };
    assert_eq!(ex.apply(withdraw, 0), Err(ExchangeError::InsufficientFunds));
    let deposit = Input::Deposit {
        account: AccountId(1),
        amount: u64::MAX,
    };
    assert_eq!(ex.apply(deposit, 0), Err(ExchangeError::DepositLimit));
    ex.ledger().assert_consistent();
}

#[test]
fn tick_expires_orders_in_every_market() {
    let mut ex = run(&setup());
    for (id, market) in [(1, 2), (2, 1)] {
        let order = NewOrder {
            id: OrderId(id),
            market: MarketId(market),
            account: AccountId(1),
            side: Buy,
            outcome: Yes,
            price: Price(5000),
            qty: Qty(SHARE),
            tif: TimeInForce::Gtd { expires_at: 50 },
            post_only: false,
        };
        ex.apply(Input::Order(Command::Place(order)), 10).unwrap();
    }
    assert!(ex.apply(Input::Tick, 49).unwrap().is_empty());
    let expired: Vec<u64> = ex
        .apply(Input::Tick, 50)
        .unwrap()
        .iter()
        .map(|e| match e {
            Event::Cancelled {
                order_id,
                reason: CancelReason::Expired,
                ..
            } => order_id.0,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(expired, vec![2, 1]);
    assert_eq!(ex.ledger().usdc(AccountId(1)).reserved, 0);
}

#[test]
fn state_is_rebuilt_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs = setup();
    inputs.push(place(1, 1, Buy, Yes, 6000, 10 * SHARE));
    inputs.push(place(2, 2, Buy, No, 4000, 4 * SHARE));
    inputs.push(place(3, 2, Sell, No, 7000, 2 * SHARE));

    let mut sequencer = Sequencer::open(dir.path()).unwrap();
    submit_all(&mut sequencer, &inputs);
    sequencer.commit().unwrap();
    let before = sequencer.exchange().encode();
    drop(sequencer);

    let sequencer = Sequencer::open(dir.path()).unwrap();
    assert_eq!(sequencer.exchange().encode(), before);
    assert_eq!(sequencer.exchange().encode(), run(&inputs).encode());
    assert_eq!(sequencer.exchange().seq(), inputs.len() as u64);
}

#[test]
fn snapshot_plus_log_gives_the_same_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs = setup();
    inputs.push(place(1, 1, Buy, Yes, 6000, 10 * SHARE));
    inputs.push(place(2, 2, Buy, No, 4000, 4 * SHARE));

    let mut sequencer = Sequencer::open(dir.path()).unwrap();
    submit_all(&mut sequencer, &inputs);
    sequencer.snapshot().unwrap();
    let tail = [
        place(3, 3, Buy, No, 4000, 8 * SHARE),
        place(4, 1, Sell, Yes, 6500, 5 * SHARE),
    ];
    submit_all(&mut sequencer, &tail);
    drop(sequencer);
    inputs.extend(tail);

    let sequencer = Sequencer::open(dir.path()).unwrap();
    assert_eq!(sequencer.exchange().encode(), run(&inputs).encode());
}

#[test]
fn torn_final_record_is_dropped_and_the_log_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs = setup();
    inputs.push(place(1, 1, Buy, Yes, 6000, 10 * SHARE));
    let lost = place(2, 2, Buy, No, 4000, 4 * SHARE);

    let mut sequencer = Sequencer::open(dir.path()).unwrap();
    submit_all(&mut sequencer, &inputs);
    submit_all(&mut sequencer, &[lost]);
    drop(sequencer);

    let wal = wal_file(dir.path());
    let len = fs::metadata(&wal).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&wal)
        .unwrap()
        .set_len(len - 3)
        .unwrap();

    let mut sequencer = Sequencer::open(dir.path()).unwrap();
    assert_eq!(sequencer.exchange().encode(), run(&inputs).encode());

    let next = place(3, 3, Buy, No, 4000, 2 * SHARE);
    submit_all(&mut sequencer, std::slice::from_ref(&next));
    drop(sequencer);
    inputs.push(next);
    let sequencer = Sequencer::open(dir.path()).unwrap();
    assert_eq!(sequencer.exchange().encode(), run(&inputs).encode());
}

#[derive(Clone, Debug)]
enum Op {
    Place {
        account: u64,
        market: u32,
        side: Side,
        outcome: Outcome,
        tick: u16,
        lots: u64,
        tif: u8,
        ttl: u64,
    },
    Cancel {
        pick: u64,
        account: u64,
    },
    Tick,
    Lifecycle {
        kind: u8,
        market: u32,
        yes_wins: bool,
    },
    Deposit {
        account: u64,
        amount: u64,
    },
    Withdraw {
        account: u64,
        amount: u64,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let place = (
        1u64..5,
        1u32..4,
        prop_oneof![Just(Buy), Just(Sell)],
        prop_oneof![Just(Yes), Just(No)],
        35u16..66,
        1u64..400,
        0u8..6,
        1u64..20,
    )
        .prop_map(
            |(account, market, side, outcome, tick, lots, tif, ttl)| Op::Place {
                account,
                market,
                side,
                outcome,
                tick,
                lots,
                tif,
                ttl,
            },
        );
    prop_oneof![
        12 => place,
        3 => (any::<u64>(), 1u64..5).prop_map(|(pick, account)| Op::Cancel { pick, account }),
        1 => Just(Op::Tick),
        1 => (0u8..4, 1u32..4, any::<bool>())
            .prop_map(|(kind, market, yes_wins)| Op::Lifecycle { kind, market, yes_wins }),
        1 => (1u64..5, 0..5 * USDC).prop_map(|(account, amount)| Op::Deposit { account, amount }),
        1 => (1u64..5, 0..5 * USDC).prop_map(|(account, amount)| Op::Withdraw { account, amount }),
    ]
}

fn inputs_from(ops: Vec<Op>) -> Vec<Input> {
    let mut inputs = setup();
    let mut next_id = 1;
    for op in ops {
        let now = inputs.len() as u64;
        inputs.push(match op {
            Op::Place {
                account,
                market,
                side,
                outcome,
                tick,
                lots,
                tif,
                ttl,
            } => {
                next_id += 1;
                Input::Order(Command::Place(NewOrder {
                    id: OrderId(next_id - 1),
                    market: MarketId(market),
                    account: AccountId(account),
                    side,
                    outcome,
                    price: Price(tick * 100),
                    qty: Qty(lots * 10_000),
                    tif: match tif {
                        0 => TimeInForce::Fok,
                        1 => TimeInForce::Fak,
                        2 => TimeInForce::Gtd {
                            expires_at: now + ttl,
                        },
                        _ => TimeInForce::Gtc,
                    },
                    post_only: false,
                }))
            }
            Op::Cancel { pick, account } => Input::Order(Command::Cancel {
                market: MarketId(1 + (pick % 2) as u32),
                order_id: OrderId(1 + pick % next_id),
                account: AccountId(account),
            }),
            Op::Tick => Input::Tick,
            Op::Lifecycle {
                kind,
                market,
                yes_wins,
            } => {
                let market = MarketId(market);
                match kind {
                    0 => Input::PauseMarket { market },
                    1 => Input::ResumeMarket { market },
                    2 => Input::CloseMarket { market },
                    _ => Input::ResolveMarket {
                        market,
                        winner: if yes_wins { Yes } else { No },
                    },
                }
            }
            Op::Deposit { account, amount } => Input::Deposit {
                account: AccountId(account),
                amount,
            },
            Op::Withdraw { account, amount } => Input::Withdraw {
                account: AccountId(account),
                amount,
            },
        });
    }
    inputs
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn recovery_matches_an_uninterrupted_run(
        ops in prop::collection::vec(op(), 1..200),
        snapshots in prop::collection::vec(any::<prop::sample::Index>(), 0..3),
        crashes in prop::collection::vec(any::<prop::sample::Index>(), 1..4),
    ) {
        let inputs = inputs_from(ops);
        let snapshots: Vec<usize> = snapshots.iter().map(|i| i.index(inputs.len())).collect();
        let crashes: Vec<usize> = crashes.iter().map(|i| i.index(inputs.len())).collect();

        let dir = tempfile::tempdir().unwrap();
        let mut reference = Exchange::new();
        let mut sequencer = Sequencer::open(dir.path()).unwrap();

        for (i, input) in inputs.iter().enumerate() {
            if snapshots.contains(&i) {
                sequencer.snapshot().unwrap();
            }
            if crashes.contains(&i) {
                drop(sequencer);
                sequencer = Sequencer::open(dir.path()).unwrap();
                prop_assert_eq!(sequencer.exchange().encode(), reference.encode());
            }
            let now = i as u64;
            let expected = reference.apply(input.clone(), now);
            let actual = sequencer.submit(input.clone(), now).unwrap();
            prop_assert_eq!(actual, expected);
        }

        drop(sequencer);
        let recovered = Sequencer::open(dir.path()).unwrap();
        prop_assert_eq!(recovered.exchange().encode(), reference.encode());
        reference.ledger().assert_consistent();
    }
}
