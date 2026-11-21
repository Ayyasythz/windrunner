use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
pub use sqlx::Error;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, FromRow, PgConnection, PgPool};
use tokio::sync::mpsc::UnboundedReceiver;
use windrunner_exchange::{Applied, Input};
use windrunner_types::{Command, Event, Fill, NewOrder, RejectReason, TimeInForce, Trade};

pub type Result<T> = std::result::Result<T, Error>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projection_cursor (
    id  BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    seq BIGINT NOT NULL
);
INSERT INTO projection_cursor (seq) VALUES (0) ON CONFLICT DO NOTHING;

CREATE TABLE IF NOT EXISTS trades (
    id            BIGINT PRIMARY KEY,
    seq           BIGINT NOT NULL,
    ts            BIGINT NOT NULL,
    market        BIGINT NOT NULL,
    yes_price     INT    NOT NULL,
    qty           BIGINT NOT NULL,
    kind          TEXT   NOT NULL,
    maker_order   BIGINT NOT NULL,
    maker_account BIGINT NOT NULL,
    maker_side    TEXT   NOT NULL,
    maker_outcome TEXT   NOT NULL,
    taker_order   BIGINT NOT NULL,
    taker_account BIGINT NOT NULL,
    taker_side    TEXT   NOT NULL,
    taker_outcome TEXT   NOT NULL
);
CREATE INDEX IF NOT EXISTS trades_market_idx ON trades (market, id DESC);
CREATE INDEX IF NOT EXISTS trades_maker_idx ON trades (maker_account, id DESC);
CREATE INDEX IF NOT EXISTS trades_taker_idx ON trades (taker_account, id DESC);

CREATE TABLE IF NOT EXISTS orders (
    order_id   BIGINT  PRIMARY KEY,
    market     BIGINT  NOT NULL,
    account    BIGINT  NOT NULL,
    side       TEXT    NOT NULL,
    outcome    TEXT    NOT NULL,
    price      INT     NOT NULL,
    qty        BIGINT  NOT NULL,
    filled     BIGINT  NOT NULL DEFAULT 0,
    tif        TEXT    NOT NULL,
    expires_at BIGINT,
    post_only  BOOLEAN NOT NULL,
    status     TEXT    NOT NULL,
    reason     TEXT,
    created_at BIGINT  NOT NULL,
    updated_at BIGINT  NOT NULL
);
CREATE INDEX IF NOT EXISTS orders_account_idx ON orders (account, order_id DESC);

CREATE TABLE IF NOT EXISTS candles (
    market BIGINT NOT NULL,
    bucket BIGINT NOT NULL,
    open   INT    NOT NULL,
    high   INT    NOT NULL,
    low    INT    NOT NULL,
    close  INT    NOT NULL,
    volume BIGINT NOT NULL,
    trades BIGINT NOT NULL,
    PRIMARY KEY (market, bucket)
);
";

const CANDLE_MS: i64 = 60_000;
const BATCH: usize = 512;

/// Timestamps are milliseconds since the Unix epoch; prices and quantities
/// use the same integer units as the exchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, FromRow)]
pub struct TradeRow {
    pub id: i64,
    pub seq: i64,
    pub ts: i64,
    pub market: i64,
    pub yes_price: i32,
    pub qty: i64,
    pub kind: String,
    pub taker_side: String,
    pub taker_outcome: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, FromRow)]
pub struct FillRow {
    pub id: i64,
    pub seq: i64,
    pub ts: i64,
    pub market: i64,
    pub yes_price: i32,
    pub qty: i64,
    pub kind: String,
    pub order_id: i64,
    pub side: String,
    pub outcome: String,
    pub role: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, FromRow)]
pub struct OrderRow {
    pub order_id: i64,
    pub market: i64,
    pub side: String,
    pub outcome: String,
    pub price: i32,
    pub qty: i64,
    pub filled: i64,
    pub tif: String,
    pub expires_at: Option<i64>,
    pub post_only: bool,
    pub status: String,
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, FromRow)]
pub struct Candle {
    pub start: i64,
    pub open: i32,
    pub high: i32,
    pub low: i32,
    pub close: i32,
    pub volume: i64,
    pub trades: i64,
}

#[derive(Clone)]
pub struct History {
    pool: PgPool,
}

fn name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl History {
    pub async fn connect(url: &str) -> Result<Self> {
        Self::open(url.parse()?).await
    }

    /// Keeps every table inside `schema`, creating it if needed.
    pub async fn connect_schema(url: &str, schema: &str) -> Result<Self> {
        let valid = !schema.is_empty()
            && schema
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(Error::Configuration("invalid schema name".into()));
        }
        let options: PgConnectOptions = url.parse()?;
        let mut conn = PgConnection::connect_with(&options).await?;
        let create = format!("CREATE SCHEMA IF NOT EXISTS {schema}");
        sqlx::raw_sql(sqlx::AssertSqlSafe(create))
            .execute(&mut conn)
            .await?;
        conn.close().await?;
        Self::open(options.options([("search_path", schema)])).await
    }

    async fn open(options: PgConnectOptions) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(History { pool })
    }

    /// Sequence number of the last input whose effects are stored.
    pub async fn cursor(&self) -> Result<u64> {
        let seq: i64 = sqlx::query_scalar("SELECT seq FROM projection_cursor")
            .fetch_one(&self.pool)
            .await?;
        Ok(seq as u64)
    }

    /// Stores records until the channel closes, retrying forever on database
    /// errors, and publishes the cursor through `projected`.
    pub async fn run(self, mut records: UnboundedReceiver<Applied>, projected: Arc<AtomicU64>) {
        let mut batch = Vec::new();
        while records.recv_many(&mut batch, BATCH).await > 0 {
            loop {
                match self.apply_batch(&batch).await {
                    Ok(cursor) => {
                        projected.store(cursor, Ordering::Release);
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "history projection failed, retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            batch.clear();
        }
    }

    /// Records at or below the stored cursor are skipped, so replaying the
    /// same input twice never duplicates anything.
    pub async fn apply_batch(&self, batch: &[Applied]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let stored: i64 = sqlx::query_scalar("SELECT seq FROM projection_cursor FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
        let mut cursor = stored as u64;
        for record in batch {
            if record.seq <= cursor {
                continue;
            }
            if record.seq != cursor + 1 {
                tracing::warn!(
                    from = cursor + 1,
                    to = record.seq - 1,
                    "history gap: inputs were never projected"
                );
            }
            project(&mut tx, record).await?;
            cursor = record.seq;
        }
        sqlx::query("UPDATE projection_cursor SET seq = $1")
            .bind(cursor as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(cursor)
    }

    pub async fn market_trades(
        &self,
        market: u32,
        before: i64,
        limit: i64,
    ) -> Result<Vec<TradeRow>> {
        sqlx::query_as(
            "SELECT id, seq, ts, market, yes_price, qty, kind, taker_side, taker_outcome
             FROM trades WHERE market = $1 AND id < $2 ORDER BY id DESC LIMIT $3",
        )
        .bind(market as i64)
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn account_fills(
        &self,
        account: u64,
        before: i64,
        limit: i64,
    ) -> Result<Vec<FillRow>> {
        sqlx::query_as(
            "SELECT id, seq, ts, market, yes_price, qty, kind,
                    CASE WHEN taker_account = $1 THEN taker_order ELSE maker_order END AS order_id,
                    CASE WHEN taker_account = $1 THEN taker_side ELSE maker_side END AS side,
                    CASE WHEN taker_account = $1 THEN taker_outcome ELSE maker_outcome END AS outcome,
                    CASE WHEN taker_account = $1 THEN 'taker' ELSE 'maker' END AS role
             FROM trades
             WHERE (maker_account = $1 OR taker_account = $1) AND id < $2
             ORDER BY id DESC LIMIT $3",
        )
        .bind(account as i64)
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn account_orders(
        &self,
        account: u64,
        status: Option<&str>,
        before: i64,
        limit: i64,
    ) -> Result<Vec<OrderRow>> {
        sqlx::query_as(
            "SELECT order_id, market, side, outcome, price, qty, filled, tif, expires_at,
                    post_only, status, reason, created_at, updated_at
             FROM orders
             WHERE account = $1 AND order_id < $2 AND ($3::text IS NULL OR status = $3)
             ORDER BY order_id DESC LIMIT $4",
        )
        .bind(account as i64)
        .bind(before)
        .bind(status)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Newest first. `interval_ms` must be a multiple of one minute.
    pub async fn candles(&self, market: u32, interval_ms: i64, limit: i64) -> Result<Vec<Candle>> {
        sqlx::query_as(
            "SELECT bucket - bucket % $2 AS start,
                    (array_agg(open ORDER BY bucket))[1] AS open,
                    max(high) AS high,
                    min(low) AS low,
                    (array_agg(close ORDER BY bucket DESC))[1] AS close,
                    sum(volume)::bigint AS volume,
                    sum(trades)::bigint AS trades
             FROM candles WHERE market = $1
             GROUP BY 1 ORDER BY 1 DESC LIMIT $3",
        )
        .bind(market as i64)
        .bind(interval_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }
}

async fn project(conn: &mut PgConnection, record: &Applied) -> Result<()> {
    let ts = record.now as i64;
    if let Input::Order(Command::Place(order)) = record.input {
        insert_order(conn, &order, &record.events, ts).await?;
    }
    for (index, event) in record.events.iter().enumerate() {
        match event {
            Event::Trade(trade) => {
                insert_trade(conn, record.seq, index, trade, ts).await?;
                fill_order(conn, &trade.maker, ts).await?;
                fill_order(conn, &trade.taker, ts).await?;
            }
            Event::Cancelled {
                order_id, reason, ..
            } => {
                sqlx::query(
                    "UPDATE orders SET status = 'cancelled', reason = $2, updated_at = $3
                     WHERE order_id = $1",
                )
                .bind(order_id.0 as i64)
                .bind(name(reason))
                .bind(ts)
                .execute(&mut *conn)
                .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn insert_order(
    conn: &mut PgConnection,
    order: &NewOrder,
    events: &[Event],
    ts: i64,
) -> Result<()> {
    let rejection = events.iter().find_map(|e| match e {
        Event::Rejected { reason, .. } => Some(*reason),
        _ => None,
    });
    // That id belongs to an earlier order which already has its row.
    if rejection == Some(RejectReason::DuplicateOrderId) {
        return Ok(());
    }
    let (tif, expires_at) = match order.tif {
        TimeInForce::Gtc => ("gtc", None),
        TimeInForce::Gtd { expires_at } => ("gtd", Some(expires_at as i64)),
        TimeInForce::Fok => ("fok", None),
        TimeInForce::Fak => ("fak", None),
    };
    sqlx::query(
        "INSERT INTO orders (order_id, market, account, side, outcome, price, qty, tif,
                             expires_at, post_only, status, reason, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $13)
         ON CONFLICT (order_id) DO NOTHING",
    )
    .bind(order.id.0 as i64)
    .bind(order.market.0 as i64)
    .bind(order.account.0 as i64)
    .bind(name(&order.side))
    .bind(name(&order.outcome))
    .bind(order.price.0 as i32)
    .bind(order.qty.0 as i64)
    .bind(tif)
    .bind(expires_at)
    .bind(order.post_only)
    .bind(if rejection.is_some() {
        "rejected"
    } else {
        "open"
    })
    .bind(rejection.map(|reason| name(&reason)))
    .bind(ts)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn fill_order(conn: &mut PgConnection, fill: &Fill, ts: i64) -> Result<()> {
    sqlx::query(
        "UPDATE orders
         SET filled = qty - $2,
             status = CASE WHEN $2 = 0 THEN 'filled' ELSE status END,
             updated_at = $3
         WHERE order_id = $1",
    )
    .bind(fill.order_id.0 as i64)
    .bind(fill.remaining.0 as i64)
    .bind(ts)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn insert_trade(
    conn: &mut PgConnection,
    seq: u64,
    index: usize,
    trade: &Trade,
    ts: i64,
) -> Result<()> {
    // One input cannot produce anywhere near 2^20 events, so this id is
    // unique and sorts in execution order.
    let id = ((seq as i64) << 20) | index as i64;
    sqlx::query(
        "INSERT INTO trades (id, seq, ts, market, yes_price, qty, kind,
                             maker_order, maker_account, maker_side, maker_outcome,
                             taker_order, taker_account, taker_side, taker_outcome)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(id)
    .bind(seq as i64)
    .bind(ts)
    .bind(trade.market.0 as i64)
    .bind(trade.yes_price.0 as i32)
    .bind(trade.qty.0 as i64)
    .bind(name(&trade.kind))
    .bind(trade.maker.order_id.0 as i64)
    .bind(trade.maker.account.0 as i64)
    .bind(name(&trade.maker.side))
    .bind(name(&trade.maker.outcome))
    .bind(trade.taker.order_id.0 as i64)
    .bind(trade.taker.account.0 as i64)
    .bind(name(&trade.taker.side))
    .bind(name(&trade.taker.outcome))
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "INSERT INTO candles (market, bucket, open, high, low, close, volume, trades)
         VALUES ($1, $2, $3, $3, $3, $3, $4, 1)
         ON CONFLICT (market, bucket) DO UPDATE
         SET high = GREATEST(candles.high, EXCLUDED.high),
             low = LEAST(candles.low, EXCLUDED.low),
             close = EXCLUDED.close,
             volume = candles.volume + EXCLUDED.volume,
             trades = candles.trades + 1",
    )
    .bind(trade.market.0 as i64)
    .bind(ts - ts.rem_euclid(CANDLE_MS))
    .bind(trade.yes_price.0 as i32)
    .bind(trade.qty.0 as i64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
