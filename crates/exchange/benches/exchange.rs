use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use windrunner_engine::BookConfig;
use windrunner_exchange::{Exchange, Input, Sequencer};
use windrunner_types::*;

const N: u64 = 50_000;
const ACCOUNTS: u64 = 1000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn setup() -> Vec<Input> {
    let mut inputs = vec![Input::CreateMarket {
        market: MarketId(1),
        config: BookConfig::default(),
    }];
    inputs.extend((1..=ACCOUNTS).map(|a| Input::Deposit {
        account: AccountId(a),
        amount: 1_000_000_000,
    }));
    inputs
}

/// Buys of both outcomes around 0.50, so crossing pairs mint shares and the
/// ledger settles real trades.
fn workload() -> Vec<Input> {
    let mut rng = Rng(11);
    (1..=N)
        .map(|id| {
            let account = AccountId(1 + rng.next() % ACCOUNTS);
            if id > 10 && rng.next().is_multiple_of(5) {
                return Input::Order(Command::Cancel {
                    market: MarketId(1),
                    order_id: OrderId(1 + rng.next() % (id - 1)),
                    account,
                });
            }
            let outcome = if rng.next().is_multiple_of(2) {
                Outcome::Yes
            } else {
                Outcome::No
            };
            Input::Order(Command::Place(NewOrder {
                id: OrderId(id),
                market: MarketId(1),
                account,
                side: Side::Buy,
                outcome,
                price: Price((45 + rng.next() % 11) as u16 * 100),
                qty: Qty((1 + rng.next() % 100) * 10_000),
                tif: TimeInForce::Gtc,
                post_only: false,
            }))
        })
        .collect()
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("exchange");
    group.throughput(Throughput::Elements(N));
    group.sample_size(20);
    let inputs = workload();

    group.bench_function("apply", |b| {
        b.iter_batched_ref(
            || {
                let mut exchange = Exchange::new();
                for input in setup() {
                    exchange.apply(input, 0).unwrap();
                }
                exchange
            },
            |exchange| {
                for input in &inputs {
                    let _ = black_box(exchange.apply(input.clone(), 0));
                }
            },
            BatchSize::LargeInput,
        )
    });

    // The journal is written but only fsynced once per 1024 inputs, the way
    // the server batches commits.
    group.bench_function("journaled", |b| {
        b.iter_batched_ref(
            || {
                let dir = tempfile::tempdir().unwrap();
                let mut sequencer = Sequencer::open(dir.path()).unwrap();
                for input in setup() {
                    sequencer.submit(input, 0).unwrap().unwrap();
                }
                (sequencer, dir)
            },
            |(sequencer, _dir)| {
                for (i, input) in inputs.iter().enumerate() {
                    let _ = black_box(sequencer.submit(input.clone(), 0).unwrap());
                    if i % 1024 == 1023 {
                        sequencer.commit().unwrap();
                    }
                }
                sequencer.commit().unwrap();
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(exchange, benches);
criterion_main!(exchange);
