use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use windrunner_engine::{BookConfig, OrderBook};
use windrunner_types::*;

const N: u64 = 100_000;

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

enum Op {
    Place(NewOrder),
    Cancel(OrderId, AccountId),
}

fn order(id: u64, account: u64, side: Side, outcome: Outcome, tick: u64, lots: u64) -> NewOrder {
    NewOrder {
        id: OrderId(id),
        market: MarketId(1),
        account: AccountId(account),
        side,
        outcome,
        price: Price(tick as u16 * 100),
        qty: Qty(lots * 10_000),
        tif: TimeInForce::Gtc,
        post_only: false,
    }
}

/// Orders cluster around 0.50 so a good share of them cross; one op in five
/// cancels an earlier order.
fn mixed_workload() -> Vec<Op> {
    let mut rng = Rng(7);
    let mut ops = Vec::with_capacity(N as usize);
    for id in 1..=N {
        if id > 10 && rng.next().is_multiple_of(5) {
            let target = 1 + rng.next() % (id - 1);
            ops.push(Op::Cancel(OrderId(target), AccountId(1 + target % 1000)));
            continue;
        }
        let side = if rng.next().is_multiple_of(2) {
            Side::Buy
        } else {
            Side::Sell
        };
        let outcome = if rng.next().is_multiple_of(2) {
            Outcome::Yes
        } else {
            Outcome::No
        };
        let tick = 40 + rng.next() % 21;
        ops.push(Op::Place(order(
            id,
            1 + id % 1000,
            side,
            outcome,
            tick,
            1 + rng.next() % 100,
        )));
    }
    ops
}

fn book() -> OrderBook {
    OrderBook::new(MarketId(1), BookConfig::default())
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine");
    group.throughput(Throughput::Elements(N));

    let ops = mixed_workload();
    group.bench_function("mixed", |b| {
        b.iter_batched_ref(
            book,
            |book| {
                for op in &ops {
                    match *op {
                        Op::Place(o) => black_box(book.place(o, 0)),
                        Op::Cancel(id, account) => black_box(book.cancel(id, account)),
                    };
                }
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function("rest_then_cancel", |b| {
        b.iter_batched_ref(
            book,
            |book| {
                for id in 1..=N / 2 {
                    black_box(book.place(order(id, 1, Side::Buy, Outcome::Yes, 1 + id % 49, 1), 0));
                }
                for id in 1..=N / 2 {
                    black_box(book.cancel(OrderId(id), AccountId(1)));
                }
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function("sweep_deep_level", |b| {
        b.iter_batched_ref(
            || {
                let mut book = book();
                for id in 1..N {
                    book.place(order(id, 1, Side::Sell, Outcome::Yes, 50, 1), 0);
                }
                book
            },
            |book| black_box(book.place(order(N, 2, Side::Buy, Outcome::Yes, 50, N), 0)),
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(matching, benches);
criterion_main!(matching);
