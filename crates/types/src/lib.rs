use serde::{Deserialize, Serialize};

pub const PRICE_SCALE: u16 = 10_000;
pub const QTY_SCALE: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Price(pub u16);

impl Price {
    pub fn complement(self) -> Price {
        Price(PRICE_SCALE - self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Qty(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MarketId(pub u32);

/// Micro-USDC. Exact as long as `qty` is a multiple of `PRICE_SCALE` units,
/// which the default lot size (0.01 share) guarantees.
pub fn notional(price: Price, qty: Qty) -> u64 {
    (qty.0 as u128 * price.0 as u128 / PRICE_SCALE as u128) as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Yes,
    No,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookSide {
    Bid,
    Ask,
}

impl BookSide {
    pub fn opposite(self) -> BookSide {
        match self {
            BookSide::Bid => BookSide::Ask,
            BookSide::Ask => BookSide::Bid,
        }
    }

    pub fn of(side: Side, outcome: Outcome) -> BookSide {
        match (side, outcome) {
            (Side::Buy, Outcome::Yes) | (Side::Sell, Outcome::No) => BookSide::Bid,
            (Side::Sell, Outcome::Yes) | (Side::Buy, Outcome::No) => BookSide::Ask,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Gtc,
    Gtd { expires_at: u64 },
    Fok,
    Fak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewOrder {
    pub id: OrderId,
    pub market: MarketId,
    pub account: AccountId,
    pub side: Side,
    pub outcome: Outcome,
    /// Price of the order's own outcome token, not the YES price.
    pub price: Price,
    pub qty: Qty,
    pub tif: TimeInForce,
    pub post_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Place(NewOrder),
    Cancel {
        market: MarketId,
        order_id: OrderId,
        account: AccountId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementKind {
    TransferYes,
    TransferNo,
    Mint,
    Merge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub account: AccountId,
    pub side: Side,
    pub outcome: Outcome,
    pub remaining: Qty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    pub market: MarketId,
    pub yes_price: Price,
    pub qty: Qty,
    pub kind: SettlementKind,
    pub maker: Fill,
    pub taker: Fill,
}

impl Trade {
    pub fn price_for(&self, outcome: Outcome) -> Price {
        match outcome {
            Outcome::Yes => self.yes_price,
            Outcome::No => self.yes_price.complement(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    UnknownMarket,
    InvalidPrice,
    InvalidQty,
    InvalidTimeInForce,
    DuplicateOrderId,
    AlreadyExpired,
    PostOnlyWouldCross,
    FokNotFillable,
    UnknownOrder,
    NotOwner,
    InsufficientFunds,
    InsufficientShares,
    MarketNotOpen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    User,
    Expired,
    SelfTrade,
    Unfilled,
    MarketClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Accepted {
        order_id: OrderId,
    },
    Rejected {
        order_id: OrderId,
        reason: RejectReason,
    },
    CancelRejected {
        order_id: OrderId,
        reason: RejectReason,
    },
    Trade(Trade),
    Rested {
        order_id: OrderId,
        remaining: Qty,
    },
    Cancelled {
        market: MarketId,
        order_id: OrderId,
        account: AccountId,
        remaining: Qty,
        reason: CancelReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    Open,
    /// New orders are refused; resting orders stay and can be cancelled.
    Paused,
    /// Trading is over and the book is empty; waiting for resolution.
    Closed,
    Resolved {
        winner: Outcome,
    },
}

/// Display-only description of a market; it has no effect on trading.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketInfo {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub closes_at: Option<u64>,
}

impl MarketInfo {
    pub fn is_valid(&self) -> bool {
        let title = self.title.trim();
        !title.is_empty()
            && self.title.chars().count() <= 200
            && self.description.chars().count() <= 4000
    }
}
