use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use slab::Slab;
use windrunner_types::{
    AccountId, BookSide, CancelReason, Event, Fill, MarketId, NewOrder, OrderId, Outcome,
    PRICE_SCALE, Price, Qty, RejectReason, SettlementKind, Side, TimeInForce, Trade,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StpMode {
    CancelTaker,
    CancelMaker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookConfig {
    pub tick: u16,
    pub lot: u64,
    pub stp: StpMode,
}

impl Default for BookConfig {
    fn default() -> Self {
        BookConfig {
            tick: 100,
            lot: 10_000,
            stp: StpMode::CancelTaker,
        }
    }
}

impl BookConfig {
    pub fn is_valid(&self) -> bool {
        self.tick > 0
            && PRICE_SCALE.is_multiple_of(self.tick)
            && PRICE_SCALE / self.tick >= 2
            && self.lot > 0
    }
}

#[derive(Serialize, Deserialize)]
pub struct BookSnapshot {
    market: MarketId,
    cfg: BookConfig,
    orders: Vec<Resting>,
}

impl BookSnapshot {
    pub fn market(&self) -> MarketId {
        self.market
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Resting {
    id: OrderId,
    account: AccountId,
    side: Side,
    outcome: Outcome,
    book_side: BookSide,
    level: usize,
    remaining: u64,
    expires_at: Option<u64>,
}

struct Node {
    order: Resting,
    prev: Option<usize>,
    next: Option<usize>,
}

#[derive(Clone, Default)]
struct Level {
    head: Option<usize>,
    tail: Option<usize>,
    qty: u64,
}

pub struct OrderBook {
    market: MarketId,
    cfg: BookConfig,
    bids: Vec<Level>,
    asks: Vec<Level>,
    best_bid: Option<usize>,
    best_ask: Option<usize>,
    nodes: Slab<Node>,
    index: HashMap<OrderId, usize>,
    expiries: BTreeSet<(u64, OrderId)>,
}

fn crosses(taker: BookSide, limit: usize, level: usize) -> bool {
    match taker {
        BookSide::Bid => level <= limit,
        BookSide::Ask => level >= limit,
    }
}

impl OrderBook {
    pub fn new(market: MarketId, cfg: BookConfig) -> Self {
        assert!(cfg.is_valid(), "invalid book config");
        let levels = (PRICE_SCALE / cfg.tick) as usize + 1;
        OrderBook {
            market,
            cfg,
            bids: vec![Level::default(); levels],
            asks: vec![Level::default(); levels],
            best_bid: None,
            best_ask: None,
            nodes: Slab::new(),
            index: HashMap::new(),
            expiries: BTreeSet::new(),
        }
    }

    /// Orders are listed in queue order per level so `restore` rebuilds the same priorities.
    pub fn snapshot(&self) -> BookSnapshot {
        let mut orders = Vec::with_capacity(self.nodes.len());
        for levels in [&self.bids, &self.asks] {
            for level in levels {
                let mut cur = level.head;
                while let Some(k) = cur {
                    orders.push(self.nodes[k].order);
                    cur = self.nodes[k].next;
                }
            }
        }
        BookSnapshot {
            market: self.market,
            cfg: self.cfg,
            orders,
        }
    }

    pub fn restore(snapshot: BookSnapshot) -> Self {
        let mut book = OrderBook::new(snapshot.market, snapshot.cfg);
        for order in snapshot.orders {
            book.insert(order);
        }
        book
    }

    pub fn market(&self) -> MarketId {
        self.market
    }

    pub fn config(&self) -> BookConfig {
        self.cfg
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.best_bid.map(|l| self.price_of(l))
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.best_ask.map(|l| self.price_of(l))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn remaining(&self, id: OrderId) -> Option<Qty> {
        self.index
            .get(&id)
            .map(|&k| Qty(self.nodes[k].order.remaining))
    }

    /// Aggregated levels in YES-price terms, best first.
    pub fn depth(&self, side: BookSide, max_levels: usize) -> Vec<(Price, Qty)> {
        let mut out = Vec::new();
        let mut level = self.best(side);
        while let Some(l) = level {
            if out.len() == max_levels {
                break;
            }
            out.push((self.price_of(l), Qty(self.levels(side)[l].qty)));
            level = self.next_level(side, l);
        }
        out
    }

    pub fn place(&mut self, o: NewOrder, now: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        self.expire_into(now, &mut ev);

        if let Err(reason) = self.validate(&o, now) {
            ev.push(Event::Rejected {
                order_id: o.id,
                reason,
            });
            return ev;
        }

        let side = BookSide::of(o.side, o.outcome);
        let yes_price = match o.outcome {
            Outcome::Yes => o.price,
            Outcome::No => o.price.complement(),
        };
        let limit = (yes_price.0 / self.cfg.tick) as usize;
        let maker_side = side.opposite();

        if o.post_only
            && self
                .best(maker_side)
                .is_some_and(|l| crosses(side, limit, l))
        {
            ev.push(Event::Rejected {
                order_id: o.id,
                reason: RejectReason::PostOnlyWouldCross,
            });
            return ev;
        }
        if o.tif == TimeInForce::Fok && self.fillable(side, limit, o.account, o.qty.0) < o.qty.0 {
            ev.push(Event::Rejected {
                order_id: o.id,
                reason: RejectReason::FokNotFillable,
            });
            return ev;
        }

        ev.push(Event::Accepted { order_id: o.id });

        let mut remaining = o.qty.0;
        let mut self_trade = false;
        while remaining > 0 {
            let Some(level) = self.best(maker_side) else {
                break;
            };
            if !crosses(side, limit, level) {
                break;
            }
            let key = self.levels(maker_side)[level]
                .head
                .expect("best level is non-empty");
            let maker = self.nodes[key].order;

            if maker.account == o.account {
                match self.cfg.stp {
                    StpMode::CancelTaker => {
                        self_trade = true;
                        break;
                    }
                    StpMode::CancelMaker => {
                        self.remove(key);
                        ev.push(Event::Cancelled {
                            market: self.market,
                            order_id: maker.id,
                            account: maker.account,
                            remaining: Qty(maker.remaining),
                            reason: CancelReason::SelfTrade,
                        });
                        continue;
                    }
                }
            }

            let fill = remaining.min(maker.remaining);
            remaining -= fill;
            let maker_left = maker.remaining - fill;
            self.nodes[key].order.remaining = maker_left;
            self.levels_mut(maker_side)[level].qty -= fill;
            if maker_left == 0 {
                self.remove(key);
            }

            let (bid_outcome, ask_outcome) = match side {
                BookSide::Bid => (o.outcome, maker.outcome),
                BookSide::Ask => (maker.outcome, o.outcome),
            };
            let kind = match (bid_outcome, ask_outcome) {
                (Outcome::Yes, Outcome::Yes) => SettlementKind::TransferYes,
                (Outcome::Yes, Outcome::No) => SettlementKind::Mint,
                (Outcome::No, Outcome::Yes) => SettlementKind::Merge,
                (Outcome::No, Outcome::No) => SettlementKind::TransferNo,
            };
            ev.push(Event::Trade(Trade {
                market: self.market,
                yes_price: self.price_of(level),
                qty: Qty(fill),
                kind,
                maker: Fill {
                    order_id: maker.id,
                    account: maker.account,
                    side: maker.side,
                    outcome: maker.outcome,
                    remaining: Qty(maker_left),
                },
                taker: Fill {
                    order_id: o.id,
                    account: o.account,
                    side: o.side,
                    outcome: o.outcome,
                    remaining: Qty(remaining),
                },
            }));
        }

        if remaining == 0 {
            return ev;
        }
        let cancel = |reason| Event::Cancelled {
            market: self.market,
            order_id: o.id,
            account: o.account,
            remaining: Qty(remaining),
            reason,
        };
        if self_trade {
            ev.push(cancel(CancelReason::SelfTrade));
            return ev;
        }
        match o.tif {
            TimeInForce::Fok | TimeInForce::Fak => ev.push(cancel(CancelReason::Unfilled)),
            TimeInForce::Gtc | TimeInForce::Gtd { .. } => {
                let expires_at = match o.tif {
                    TimeInForce::Gtd { expires_at } => Some(expires_at),
                    _ => None,
                };
                self.insert(Resting {
                    id: o.id,
                    account: o.account,
                    side: o.side,
                    outcome: o.outcome,
                    book_side: side,
                    level: limit,
                    remaining,
                    expires_at,
                });
                ev.push(Event::Rested {
                    order_id: o.id,
                    remaining: Qty(remaining),
                });
            }
        }
        ev
    }

    pub fn cancel(&mut self, order_id: OrderId, account: AccountId) -> Vec<Event> {
        let Some(&key) = self.index.get(&order_id) else {
            return vec![Event::CancelRejected {
                order_id,
                reason: RejectReason::UnknownOrder,
            }];
        };
        if self.nodes[key].order.account != account {
            return vec![Event::CancelRejected {
                order_id,
                reason: RejectReason::NotOwner,
            }];
        }
        let order = self.remove(key);
        vec![Event::Cancelled {
            market: self.market,
            order_id,
            account,
            remaining: Qty(order.remaining),
            reason: CancelReason::User,
        }]
    }

    pub fn cancel_all(&mut self, reason: CancelReason) -> Vec<Event> {
        let orders = self.snapshot().orders;
        let mut ev = Vec::with_capacity(orders.len());
        for order in orders {
            self.remove(self.index[&order.id]);
            ev.push(Event::Cancelled {
                market: self.market,
                order_id: order.id,
                account: order.account,
                remaining: Qty(order.remaining),
                reason,
            });
        }
        ev
    }

    pub fn expire(&mut self, now: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        self.expire_into(now, &mut ev);
        ev
    }

    fn expire_into(&mut self, now: u64, ev: &mut Vec<Event>) {
        while let Some(&(at, id)) = self.expiries.first() {
            if at > now {
                break;
            }
            let order = self.remove(self.index[&id]);
            ev.push(Event::Cancelled {
                market: self.market,
                order_id: id,
                account: order.account,
                remaining: Qty(order.remaining),
                reason: CancelReason::Expired,
            });
        }
    }

    fn validate(&self, o: &NewOrder, now: u64) -> Result<(), RejectReason> {
        let tick = self.cfg.tick;
        if !o.price.0.is_multiple_of(tick) || o.price.0 < tick || o.price.0 > PRICE_SCALE - tick {
            return Err(RejectReason::InvalidPrice);
        }
        if o.qty.0 == 0 || !o.qty.0.is_multiple_of(self.cfg.lot) {
            return Err(RejectReason::InvalidQty);
        }
        match o.tif {
            TimeInForce::Gtd { expires_at } if expires_at <= now => {
                return Err(RejectReason::AlreadyExpired);
            }
            TimeInForce::Fok | TimeInForce::Fak if o.post_only => {
                return Err(RejectReason::InvalidTimeInForce);
            }
            _ => {}
        }
        if self.index.contains_key(&o.id) {
            return Err(RejectReason::DuplicateOrderId);
        }
        Ok(())
    }

    fn fillable(&self, taker: BookSide, limit: usize, account: AccountId, want: u64) -> u64 {
        let maker_side = taker.opposite();
        let mut sum = 0u64;
        let mut level = self.best(maker_side);
        while let Some(l) = level {
            if !crosses(taker, limit, l) {
                break;
            }
            let mut cur = self.levels(maker_side)[l].head;
            while let Some(k) = cur {
                let node = &self.nodes[k];
                if node.order.account == account {
                    if self.cfg.stp == StpMode::CancelTaker {
                        return sum;
                    }
                } else {
                    sum += node.order.remaining;
                    if sum >= want {
                        return sum;
                    }
                }
                cur = node.next;
            }
            level = self.next_level(maker_side, l);
        }
        sum
    }

    fn insert(&mut self, order: Resting) {
        let (side, level) = (order.book_side, order.level);
        let tail = self.levels(side)[level].tail;
        let key = self.nodes.insert(Node {
            order,
            prev: tail,
            next: None,
        });
        if let Some(t) = tail {
            self.nodes[t].next = Some(key);
        }
        let lvl = &mut self.levels_mut(side)[level];
        if tail.is_none() {
            lvl.head = Some(key);
        }
        lvl.tail = Some(key);
        lvl.qty += order.remaining;

        self.index.insert(order.id, key);
        if let Some(at) = order.expires_at {
            self.expiries.insert((at, order.id));
        }
        match side {
            BookSide::Bid if self.best_bid.is_none_or(|b| level > b) => self.best_bid = Some(level),
            BookSide::Ask if self.best_ask.is_none_or(|b| level < b) => self.best_ask = Some(level),
            _ => {}
        }
    }

    fn remove(&mut self, key: usize) -> Resting {
        let Node { order, prev, next } = self.nodes.remove(key);
        if let Some(p) = prev {
            self.nodes[p].next = next;
        }
        if let Some(n) = next {
            self.nodes[n].prev = prev;
        }
        let lvl = &mut self.levels_mut(order.book_side)[order.level];
        if prev.is_none() {
            lvl.head = next;
        }
        if next.is_none() {
            lvl.tail = prev;
        }
        lvl.qty -= order.remaining;
        let emptied = lvl.head.is_none();

        self.index.remove(&order.id);
        if let Some(at) = order.expires_at {
            self.expiries.remove(&(at, order.id));
        }
        if emptied && self.best(order.book_side) == Some(order.level) {
            let next_best = self.next_level(order.book_side, order.level);
            match order.book_side {
                BookSide::Bid => self.best_bid = next_best,
                BookSide::Ask => self.best_ask = next_best,
            }
        }
        order
    }

    fn next_level(&self, side: BookSide, from: usize) -> Option<usize> {
        match side {
            BookSide::Bid => (0..from).rev().find(|&i| self.bids[i].head.is_some()),
            BookSide::Ask => (from + 1..self.asks.len()).find(|&i| self.asks[i].head.is_some()),
        }
    }

    fn best(&self, side: BookSide) -> Option<usize> {
        match side {
            BookSide::Bid => self.best_bid,
            BookSide::Ask => self.best_ask,
        }
    }

    fn levels(&self, side: BookSide) -> &[Level] {
        match side {
            BookSide::Bid => &self.bids,
            BookSide::Ask => &self.asks,
        }
    }

    fn levels_mut(&mut self, side: BookSide) -> &mut [Level] {
        match side {
            BookSide::Bid => &mut self.bids,
            BookSide::Ask => &mut self.asks,
        }
    }

    fn price_of(&self, level: usize) -> Price {
        Price(level as u16 * self.cfg.tick)
    }

    #[doc(hidden)]
    pub fn assert_consistent(&self) {
        let mut seen = 0;
        for side in [BookSide::Bid, BookSide::Ask] {
            let levels = self.levels(side);
            for (i, lvl) in levels.iter().enumerate() {
                let (mut sum, mut prev, mut cur) = (0u64, None, lvl.head);
                while let Some(k) = cur {
                    let node = &self.nodes[k];
                    assert_eq!(node.prev, prev);
                    assert_eq!((node.order.book_side, node.order.level), (side, i));
                    assert!(node.order.remaining > 0);
                    assert_eq!(self.index.get(&node.order.id), Some(&k));
                    sum += node.order.remaining;
                    seen += 1;
                    prev = cur;
                    cur = node.next;
                }
                assert_eq!(lvl.tail, prev);
                assert_eq!(lvl.qty, sum);
            }
            let first = match side {
                BookSide::Bid => levels.iter().rposition(|l| l.head.is_some()),
                BookSide::Ask => levels.iter().position(|l| l.head.is_some()),
            };
            assert_eq!(self.best(side), first);
        }
        assert_eq!(seen, self.nodes.len());
        assert_eq!(seen, self.index.len());
        if let (Some(b), Some(a)) = (self.best_bid, self.best_ask) {
            assert!(b < a, "book is crossed: bid {b} >= ask {a}");
        }
    }
}
