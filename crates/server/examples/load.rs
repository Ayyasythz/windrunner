//! Drives the exchange thread the way the HTTP handlers do, without HTTP:
//! many concurrent clients, each waiting for its order to be fsynced.
//!
//!     cargo run --release -p windrunner-server --example load -- [clients] [orders_per_client]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windrunner_engine::BookConfig;
use windrunner_exchange::Input;
use windrunner_server::actor::{self, Config, Handle, HandleError};
use windrunner_types::*;

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

async fn client(handle: Handle, account: u64, orders: u64) -> Vec<Duration> {
    let mut rng = Rng(account);
    let mut latencies = Vec::with_capacity(orders as usize);
    for _ in 0..orders {
        let outcome = if rng.next().is_multiple_of(2) {
            Outcome::Yes
        } else {
            Outcome::No
        };
        let order = NewOrder {
            id: OrderId(0),
            market: MarketId(1),
            account: AccountId(account),
            side: Side::Buy,
            outcome,
            price: Price((45 + rng.next() % 11) as u16 * 100),
            qty: Qty((1 + rng.next() % 100) * 10_000),
            tif: TimeInForce::Gtc,
            post_only: false,
        };
        let started = Instant::now();
        loop {
            match handle.submit(Input::Order(Command::Place(order))).await {
                Ok(_) => break,
                Err(HandleError::Busy) => tokio::task::yield_now().await,
                Err(HandleError::Stopped) => panic!("exchange stopped"),
            }
        }
        latencies.push(started.elapsed());
    }
    latencies
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args()
        .skip(1)
        .map(|a| a.parse::<u64>().expect("numeric argument"));
    let clients = args.next().unwrap_or(256);
    let orders = args.next().unwrap_or(2_000);

    let dir = tempfile::tempdir().unwrap();
    let handle = actor::spawn(dir.path(), Config::default(), None).unwrap();
    let market = Input::CreateMarket {
        market: MarketId(1),
        config: BookConfig::default(),
    };
    handle.submit(market).await.unwrap().result.unwrap();
    for account in 1..=clients {
        let deposit = Input::Deposit {
            account: AccountId(account),
            amount: 100_000_000_000,
        };
        handle.submit(deposit).await.unwrap().result.unwrap();
    }
    let metrics = handle.metrics();
    let batches_before = metrics.batches.load(Ordering::Relaxed);

    let started = Instant::now();
    let tasks: Vec<_> = (1..=clients)
        .map(|account| tokio::spawn(client(handle.clone(), account, orders)))
        .collect();
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    let elapsed = started.elapsed();

    latencies.sort_unstable();
    let total = latencies.len();
    let percentile = |p: f64| latencies[((total - 1) as f64 * p) as usize];
    let batches = metrics.batches.load(Ordering::Relaxed) - batches_before;
    println!("clients            {clients}");
    println!("orders             {total}");
    println!("elapsed            {elapsed:.2?}");
    println!(
        "throughput         {:.0} orders/s",
        total as f64 / elapsed.as_secs_f64()
    );
    println!(
        "trades             {}",
        metrics.trades.load(Ordering::Relaxed)
    );
    println!(
        "commits (fsync)    {batches}, {:.1} orders each",
        total as f64 / batches.max(1) as f64
    );
    println!(
        "queue rejections   {}",
        metrics.queue_rejections.load(Ordering::Relaxed)
    );
    println!("latency p50        {:.2?}", percentile(0.50));
    println!("latency p99        {:.2?}", percentile(0.99));
    println!("latency max        {:.2?}", percentile(1.0));
}
