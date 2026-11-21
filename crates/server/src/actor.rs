use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{broadcast, oneshot};
use windrunner_engine::OrderBook;
use windrunner_exchange::{Applied, Exchange, ExchangeError, Input, Sequencer};
use windrunner_types::{AccountId, BookSide, Command, Event, MarketId, NewOrder, OrderId};

use crate::metrics::Metrics;

const MAX_BATCH: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub tick_interval: Duration,
    pub snapshot_every: u64,
    pub queue: usize,
    pub feed_capacity: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tick_interval: Duration::from_secs(1),
            snapshot_every: 100_000,
            queue: 8192,
            feed_capacity: 4096,
        }
    }
}

/// Where the exchange reports every applied input for the history database.
pub struct Projection {
    pub sink: UnboundedSender<Applied>,
    /// Sequence number of the last input the database has stored.
    pub projected: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Topic {
    Book(MarketId),
    Trades(MarketId),
    User(AccountId),
}

#[derive(Clone, Debug)]
pub struct Feed {
    pub topic: Topic,
    pub json: Arc<str>,
}

#[derive(Debug)]
pub struct Submitted {
    pub seq: u64,
    pub order_id: Option<OrderId>,
    pub result: Result<Vec<Event>, ExchangeError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleError {
    Busy,
    Stopped,
}

type QueryFn = Box<dyn FnOnce(&Exchange) + Send>;

enum Request {
    Submit {
        input: Input,
        reply: oneshot::Sender<Submitted>,
    },
    Query(QueryFn),
}

#[derive(Clone)]
pub struct Handle {
    tx: SyncSender<Request>,
    feed: broadcast::Sender<Feed>,
    metrics: Arc<Metrics>,
}

impl Handle {
    /// Resolves only after the input is fsynced. The id of a placed order is
    /// assigned here and returned in `Submitted::order_id`.
    pub async fn submit(&self, input: Input) -> Result<Submitted, HandleError> {
        let (reply, rx) = oneshot::channel();
        self.send(Request::Submit { input, reply })?;
        rx.await.map_err(|_| HandleError::Stopped)
    }

    pub async fn query<T, F>(&self, f: F) -> Result<T, HandleError>
    where
        T: Send + 'static,
        F: FnOnce(&Exchange) -> T + Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        self.send(Request::Query(Box::new(move |exchange| {
            let _ = reply.send(f(exchange));
        })))?;
        rx.await.map_err(|_| HandleError::Stopped)
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Feed> {
        self.feed.subscribe()
    }

    fn send(&self, request: Request) -> Result<(), HandleError> {
        self.tx.try_send(request).map_err(|e| match e {
            TrySendError::Full(_) => {
                self.metrics
                    .queue_rejections
                    .fetch_add(1, Ordering::Relaxed);
                HandleError::Busy
            }
            TrySendError::Disconnected(_) => HandleError::Stopped,
        })
    }
}

/// Recovers the exchange from `dir` and runs it on its own thread until every
/// `Handle` is dropped.
pub fn spawn(
    dir: impl AsRef<Path>,
    config: Config,
    projection: Option<Projection>,
) -> io::Result<Handle> {
    let sequencer = Sequencer::open_with(dir, |applied| {
        if let Some(projection) = &projection {
            let _ = projection.sink.send(applied);
        }
    })?;
    let seq = sequencer.exchange().seq();
    tracing::info!(seq, "exchange recovered");
    let metrics = Arc::new(Metrics::default());
    metrics.sequence.store(seq, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel(config.queue);
    let (feed, _) = broadcast::channel(config.feed_capacity);
    let actor = Actor {
        sequencer,
        feed: feed.clone(),
        metrics: metrics.clone(),
        config,
        since_snapshot: 0,
        projection,
    };
    std::thread::Builder::new()
        .name("exchange".into())
        .spawn(move || {
            if let Err(e) = actor.run(rx) {
                tracing::error!(error = %e, "exchange stopped: journal failure");
            }
        })?;
    Ok(Handle { tx, feed, metrics })
}

pub fn book_json(book: &OrderBook, seq: u64, depth: usize) -> Value {
    json!({
        "market": book.market(),
        "seq": seq,
        "bids": book.depth(BookSide::Bid, depth),
        "asks": book.depth(BookSide::Ask, depth),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Default)]
struct Outbox {
    feeds: Vec<Feed>,
    dirty: BTreeSet<MarketId>,
}

struct Actor {
    sequencer: Sequencer,
    feed: broadcast::Sender<Feed>,
    metrics: Arc<Metrics>,
    config: Config,
    since_snapshot: u64,
    projection: Option<Projection>,
}

impl Actor {
    fn run(mut self, rx: mpsc::Receiver<Request>) -> io::Result<()> {
        let mut last_tick = Instant::now();
        loop {
            let mut batch = Vec::new();
            match rx.recv_timeout(self.config.tick_interval) {
                Ok(request) => batch.push(request),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            while batch.len() < MAX_BATCH {
                match rx.try_recv() {
                    Ok(request) => batch.push(request),
                    Err(_) => break,
                }
            }

            // After a crash only the inputs logged since the last snapshot
            // are replayed, so the history database must already hold
            // everything older before a snapshot may discard it.
            if self.since_snapshot >= self.config.snapshot_every && self.history_caught_up() {
                self.sequencer.snapshot()?;
                self.since_snapshot = 0;
            }

            let mut outbox = Outbox::default();
            let mut replies = Vec::new();
            let mut queries = Vec::new();
            let mut submitted = 0;
            for request in batch {
                match request {
                    Request::Submit { input, reply } => {
                        replies.push((reply, self.submit(input, &mut outbox)?));
                        submitted += 1;
                    }
                    Request::Query(query) => queries.push(query),
                }
            }
            if last_tick.elapsed() >= self.config.tick_interval {
                last_tick = Instant::now();
                if self.has_markets() {
                    self.submit(Input::Tick, &mut outbox)?;
                    submitted += 1;
                }
            }

            // Nothing leaves the process (replies, feeds, query results) until
            // the inputs behind it are on disk.
            if submitted > 0 {
                let started = Instant::now();
                self.sequencer.commit()?;
                self.metrics.record_batch(submitted, started.elapsed());
            }
            for (reply, result) in replies {
                let _ = reply.send(result);
            }
            for query in queries {
                query(self.sequencer.exchange());
            }
            self.publish(outbox);

            self.since_snapshot += submitted;
            let seq = self.sequencer.exchange().seq();
            self.metrics.sequence.store(seq, Ordering::Relaxed);
            if let Some(projection) = &self.projection {
                let stored = projection.projected.load(Ordering::Acquire);
                self.metrics
                    .history_sequence
                    .store(stored, Ordering::Relaxed);
            }
        }
    }

    fn history_caught_up(&self) -> bool {
        self.projection
            .as_ref()
            .is_none_or(|p| p.projected.load(Ordering::Acquire) >= self.sequencer.exchange().seq())
    }

    fn has_markets(&self) -> bool {
        self.sequencer.exchange().engine().books().next().is_some()
    }

    fn submit(&mut self, input: Input, outbox: &mut Outbox) -> io::Result<Submitted> {
        let seq = self.sequencer.exchange().seq() + 1;
        // The sequence number is unique and survives restarts, which makes it
        // a ready-made order id.
        let (input, order_id) = match input {
            Input::Order(Command::Place(order)) => {
                let id = OrderId(seq);
                let order = NewOrder { id, ..order };
                (Input::Order(Command::Place(order)), Some(id))
            }
            other => (other, None),
        };
        let now = now_ms();
        let result = self.sequencer.submit(input.clone(), now)?;
        if let Some(projection) = &self.projection {
            let _ = projection.sink.send(Applied {
                seq,
                now,
                input: input.clone(),
                events: result.clone().unwrap_or_default(),
            });
        }
        if let Ok(events) = &result {
            collect(seq, &input, events, outbox);
            let trades = events
                .iter()
                .filter(|e| matches!(e, Event::Trade(_)))
                .count();
            self.metrics
                .trades
                .fetch_add(trades as u64, Ordering::Relaxed);
        }
        Ok(Submitted {
            seq,
            order_id,
            result,
        })
    }

    fn publish(&self, outbox: Outbox) {
        if self.feed.receiver_count() == 0 {
            return;
        }
        for feed in outbox.feeds {
            let _ = self.feed.send(feed);
        }
        let exchange = self.sequencer.exchange();
        for market in outbox.dirty {
            if let Some(book) = exchange.engine().book(market) {
                let mut json = book_json(book, exchange.seq(), usize::MAX);
                json["channel"] = "book".into();
                let _ = self.feed.send(Feed {
                    topic: Topic::Book(market),
                    json: json.to_string().into(),
                });
            }
        }
    }
}

fn collect(seq: u64, input: &Input, events: &[Event], outbox: &mut Outbox) {
    let origin = match *input {
        Input::Order(Command::Place(o)) => Some((o.account, o.market)),
        Input::Order(Command::Cancel {
            account, market, ..
        }) => Some((account, market)),
        _ => None,
    };
    for event in events {
        let mut to_user = |account: AccountId| {
            let json = json!({ "channel": "user", "seq": seq, "data": event });
            outbox.feeds.push(Feed {
                topic: Topic::User(account),
                json: json.to_string().into(),
            });
        };
        match *event {
            Event::Trade(trade) => {
                to_user(trade.maker.account);
                to_user(trade.taker.account);
                let json = json!({
                    "channel": "trades",
                    "market": trade.market,
                    "seq": seq,
                    "data": trade,
                });
                outbox.feeds.push(Feed {
                    topic: Topic::Trades(trade.market),
                    json: json.to_string().into(),
                });
                outbox.dirty.insert(trade.market);
            }
            Event::Cancelled {
                market, account, ..
            } => {
                to_user(account);
                outbox.dirty.insert(market);
            }
            Event::Accepted { .. }
            | Event::Rejected { .. }
            | Event::CancelRejected { .. }
            | Event::Rested { .. } => {
                if let Some((account, market)) = origin {
                    to_user(account);
                    if matches!(event, Event::Rested { .. }) {
                        outbox.dirty.insert(market);
                    }
                }
            }
        }
    }
}
