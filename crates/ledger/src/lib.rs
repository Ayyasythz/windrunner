use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use windrunner_types::{
    AccountId, Event, Fill, MarketId, NewOrder, OrderId, Outcome, Price, Qty, RejectReason,
    SettlementKind, Side, notional,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub available: u64,
    pub reserved: u64,
}

impl Balance {
    pub fn total(&self) -> u64 {
        self.available + self.reserved
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    InsufficientFunds,
    InsufficientShares,
    DuplicateOrder,
    UnknownOrder,
    OpenOrdersRemain,
    DepositLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OpenOrderInfo {
    pub order_id: OrderId,
    pub market: MarketId,
    pub side: Side,
    pub outcome: Outcome,
    pub price: Price,
    pub remaining: Qty,
}

#[derive(Serialize, Deserialize)]
struct OpenOrder {
    account: AccountId,
    market: MarketId,
    side: Side,
    outcome: Outcome,
    price: Price,
    remaining: u64,
    /// Micro-USDC for buys, shares of `outcome` for sells.
    reserved: u64,
}

/// Amounts are micro-USDC and micro-shares; one full share pays out one USDC.
#[derive(Default, Serialize, Deserialize)]
pub struct Ledger {
    usdc: BTreeMap<AccountId, Balance>,
    positions: BTreeMap<(AccountId, MarketId, Outcome), Balance>,
    orders: BTreeMap<OrderId, OpenOrder>,
    collateral: BTreeMap<MarketId, u64>,
    net_deposits: u64,
}

/// Every balance and every resting quantity is bounded by total deposits
/// (at most 100 shares per USDC at the lowest price), so this cap keeps all
/// u64 sums in the ledger and the order book far from overflow.
pub const MAX_NET_DEPOSITS: u64 = 1_000_000_000_000_000;

fn sub(value: &mut u64, by: u64) {
    *value = value.checked_sub(by).expect("ledger underflow");
}

fn lock(balance: Option<&mut Balance>, amount: u64) -> bool {
    match balance {
        Some(b) if b.available >= amount => {
            b.available -= amount;
            b.reserved += amount;
            true
        }
        _ => amount == 0,
    }
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usdc(&self, account: AccountId) -> Balance {
        self.usdc.get(&account).copied().unwrap_or_default()
    }

    pub fn position(&self, account: AccountId, market: MarketId, outcome: Outcome) -> Balance {
        self.positions
            .get(&(account, market, outcome))
            .copied()
            .unwrap_or_default()
    }

    pub fn collateral(&self, market: MarketId) -> u64 {
        self.collateral.get(&market).copied().unwrap_or(0)
    }

    pub fn positions_of(&self, account: AccountId) -> Vec<(MarketId, Outcome, Balance)> {
        let first = (account, MarketId(0), Outcome::Yes);
        let last = (account, MarketId(u32::MAX), Outcome::No);
        self.positions
            .range(first..=last)
            .map(|(&(_, market, outcome), &balance)| (market, outcome, balance))
            .collect()
    }

    pub fn open_orders(&self, account: AccountId) -> Vec<OpenOrderInfo> {
        self.orders
            .iter()
            .filter(|(_, o)| o.account == account)
            .map(|(&order_id, o)| OpenOrderInfo {
                order_id,
                market: o.market,
                side: o.side,
                outcome: o.outcome,
                price: o.price,
                remaining: Qty(o.remaining),
            })
            .collect()
    }

    pub fn deposit(&mut self, account: AccountId, amount: u64) -> Result<(), LedgerError> {
        let total = self
            .net_deposits
            .checked_add(amount)
            .filter(|&t| t <= MAX_NET_DEPOSITS)
            .ok_or(LedgerError::DepositLimit)?;
        self.net_deposits = total;
        self.usdc.entry(account).or_default().available += amount;
        Ok(())
    }

    pub fn withdraw(&mut self, account: AccountId, amount: u64) -> Result<(), LedgerError> {
        match self.usdc.get_mut(&account) {
            Some(b) if b.available >= amount => {
                b.available -= amount;
                self.net_deposits -= amount;
                Ok(())
            }
            _ => Err(LedgerError::InsufficientFunds),
        }
    }

    /// Must succeed before the order is sent to the engine.
    pub fn reserve(&mut self, o: &NewOrder) -> Result<(), LedgerError> {
        if self.orders.contains_key(&o.id) {
            return Err(LedgerError::DuplicateOrder);
        }
        let reserved = match o.side {
            Side::Buy => {
                let need = notional(o.price, o.qty);
                if !lock(self.usdc.get_mut(&o.account), need) {
                    return Err(LedgerError::InsufficientFunds);
                }
                need
            }
            Side::Sell => {
                let key = (o.account, o.market, o.outcome);
                if !lock(self.positions.get_mut(&key), o.qty.0) {
                    return Err(LedgerError::InsufficientShares);
                }
                o.qty.0
            }
        };
        self.orders.insert(
            o.id,
            OpenOrder {
                account: o.account,
                market: o.market,
                side: o.side,
                outcome: o.outcome,
                price: o.price,
                remaining: o.qty.0,
                reserved,
            },
        );
        Ok(())
    }

    pub fn apply(&mut self, event: &Event) -> Result<(), LedgerError> {
        match *event {
            Event::Trade(t) => {
                if !self.orders.contains_key(&t.maker.order_id)
                    || !self.orders.contains_key(&t.taker.order_id)
                {
                    return Err(LedgerError::UnknownOrder);
                }
                self.fill(t.market, &t.maker, t.price_for(t.maker.outcome), t.qty);
                self.fill(t.market, &t.taker, t.price_for(t.taker.outcome), t.qty);
                let collateral = self.collateral.entry(t.market).or_default();
                match t.kind {
                    SettlementKind::Mint => *collateral += t.qty.0,
                    SettlementKind::Merge => sub(collateral, t.qty.0),
                    SettlementKind::TransferYes | SettlementKind::TransferNo => {}
                }
                Ok(())
            }
            Event::Cancelled { order_id, .. } => self.release(order_id),
            // `reserve` already refuses duplicate ids, so this id belongs to an
            // earlier live order whose funds must stay locked.
            Event::Rejected {
                reason: RejectReason::DuplicateOrderId,
                ..
            } => Ok(()),
            Event::Rejected { order_id, .. } => self.release(order_id),
            Event::Accepted { .. } | Event::Rested { .. } | Event::CancelRejected { .. } => Ok(()),
        }
    }

    /// Pays one USDC per winning share and clears the market's positions.
    pub fn settle_market(&mut self, market: MarketId, winner: Outcome) -> Result<u64, LedgerError> {
        if self.orders.values().any(|o| o.market == market) {
            return Err(LedgerError::OpenOrdersRemain);
        }
        let mut paid = 0;
        let usdc = &mut self.usdc;
        self.positions.retain(|&(account, m, outcome), balance| {
            if m != market {
                return true;
            }
            if outcome == winner {
                usdc.entry(account).or_default().available += balance.available;
                paid += balance.available;
            }
            false
        });
        if let Some(mut collateral) = self.collateral.remove(&market) {
            sub(&mut collateral, paid);
        }
        Ok(paid)
    }

    fn fill(&mut self, market: MarketId, fill: &Fill, exec: Price, qty: Qty) {
        let order = self
            .orders
            .get_mut(&fill.order_id)
            .expect("checked by apply");
        sub(&mut order.remaining, qty.0);
        let done = order.remaining == 0;
        let (account, side, outcome) = (order.account, order.side, order.outcome);
        let proceeds = notional(exec, qty);
        match side {
            Side::Buy => {
                let locked = notional(order.price, qty);
                sub(&mut order.reserved, locked);
                let usdc = self.usdc.entry(account).or_default();
                sub(&mut usdc.reserved, locked);
                usdc.available += locked.checked_sub(proceeds).expect("fill above buy limit");
                self.positions
                    .entry((account, market, outcome))
                    .or_default()
                    .available += qty.0;
            }
            Side::Sell => {
                sub(&mut order.reserved, qty.0);
                let position = self
                    .positions
                    .entry((account, market, outcome))
                    .or_default();
                sub(&mut position.reserved, qty.0);
                self.usdc.entry(account).or_default().available += proceeds;
            }
        }
        if done {
            self.release(fill.order_id).expect("order exists");
        }
    }

    fn release(&mut self, order_id: OrderId) -> Result<(), LedgerError> {
        let order = self
            .orders
            .remove(&order_id)
            .ok_or(LedgerError::UnknownOrder)?;
        let balance = match order.side {
            Side::Buy => self.usdc.entry(order.account).or_default(),
            Side::Sell => self
                .positions
                .entry((order.account, order.market, order.outcome))
                .or_default(),
        };
        sub(&mut balance.reserved, order.reserved);
        balance.available += order.reserved;
        Ok(())
    }

    #[doc(hidden)]
    pub fn assert_consistent(&self) {
        let mut usdc_reserved: HashMap<AccountId, u64> = HashMap::new();
        let mut shares_reserved: HashMap<(AccountId, MarketId, Outcome), u64> = HashMap::new();
        for o in self.orders.values() {
            match o.side {
                Side::Buy => *usdc_reserved.entry(o.account).or_default() += o.reserved,
                Side::Sell => {
                    *shares_reserved
                        .entry((o.account, o.market, o.outcome))
                        .or_default() += o.reserved
                }
            }
        }
        for (account, balance) in &self.usdc {
            let expected = usdc_reserved.get(account).copied().unwrap_or(0);
            assert_eq!(balance.reserved, expected, "usdc reserved of {account:?}");
        }
        let mut supply: HashMap<(MarketId, Outcome), u64> = HashMap::new();
        for (key, balance) in &self.positions {
            let expected = shares_reserved.get(key).copied().unwrap_or(0);
            assert_eq!(balance.reserved, expected, "shares reserved of {key:?}");
            *supply.entry((key.1, key.2)).or_default() += balance.total();
        }
        for (&(market, outcome), &total) in &supply {
            assert_eq!(
                total,
                self.collateral(market),
                "{outcome:?} supply of {market:?}"
            );
        }
        let held: u64 = self.usdc.values().map(Balance::total).sum();
        let locked: u64 = self.collateral.values().sum();
        assert_eq!(held + locked, self.net_deposits, "usdc is not conserved");
    }
}
