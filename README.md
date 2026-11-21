# windrunner

A matching engine for binary (YES/NO) prediction markets, written in Rust.

## Layout

| Crate | Contents |
|-------|----------|
| `crates/types` | Core types: `Price`, `Qty`, orders, commands, events |
| `crates/engine` | Order book and matching, no IO |
| `crates/ledger` | Balances, reservations, mint/merge/transfer settlement, payouts |
| `crates/journal` | On-disk write-ahead log and snapshots |
| `crates/exchange` | `Exchange` (engine + ledger) and `Sequencer` (log, then apply) |
| `crates/history` | Postgres projection: order, trade and candle history |
| `crates/server` | HTTP + WebSocket server (axum) |

## Running

```sh
WINDRUNNER_ADMIN_KEY=secret \
WINDRUNNER_API_KEYS=alice-key:1,bob-key:2 \
cargo run -p windrunner-server
```

| Variable | Default | Meaning |
|----------|---------|---------|
| `WINDRUNNER_ADDR` | `127.0.0.1:8080` | Listen address |
| `WINDRUNNER_DATA_DIR` | `data` | Directory for the WAL and snapshots |
| `WINDRUNNER_ADMIN_KEY` | required | Key for the `/admin` endpoints |
| `WINDRUNNER_API_KEYS` | empty | Comma-separated `key:account_id` pairs |
| `WINDRUNNER_CORS_ORIGINS` | unset | Allowed browser origins, comma-separated, or `*`. Without it no CORS headers are sent |
| `WINDRUNNER_DATABASE_URL` | unset | Postgres URL for history, e.g. `postgres://user@localhost/windrunner`. Without it the history endpoints answer 503 `history_disabled` |

## Units

Every number in the API is an integer.

- **Price**: basis points of 1 USDC. `6000` means 0.60. An order's price is the price of its own outcome, so Buy NO @ 4000 pays 0.40 per NO share.
- **Qty**: micro-shares. `1000000` is 1 share.
- **USDC**: micro-USDC. `1000000` is 1 USDC.
- **Time** (`expires_at`): milliseconds since the Unix epoch.
- **Order book** is always quoted in YES prices: `bids` and `asks` hold `[price, qty]` pairs, best first.

## REST

Authenticate with an `Authorization: Bearer <key>` header. Errors always look like `{"error": "<code>"}`.

| Method | Path | Auth | Result |
|--------|------|------|--------|
| GET | `/health` | - | `ok` |
| GET | `/metrics` | - | Prometheus metrics |
| GET | `/markets` | - | Markets with status, `info`, tick, lot, best bid/ask |
| GET | `/markets/{market}/book?depth=N` | - | `{market, seq, bids, asks}` |
| POST | `/orders` | user | 201 `{order_id, seq, events}` |
| DELETE | `/markets/{market}/orders/{order_id}` | user | 200 `{seq, events}` |
| GET | `/account` | user | `{account, usdc, positions, open_orders}` |
| POST | `/withdrawals` | user | 200 `{seq}` |
| GET | `/orders?status=&before=&limit=` | user | `{orders}`: the account's order history, newest first |
| GET | `/fills?before=&limit=` | user | `{fills}`: the account's trades, with a maker/taker `role` |
| GET | `/markets/{market}/trades?before=&limit=` | - | `{trades}`: public trades without account identities |
| GET | `/markets/{market}/candles?interval=&limit=` | - | `{candles}`: OHLC of the YES price, newest first |
| POST | `/admin/markets` | admin | 201 `{market, tick, lot}`; the body may include `info` |
| PUT | `/admin/markets/{market}/info` | admin | 200 `{seq, events}`, body `{title, description, closes_at}` |
| POST | `/admin/deposits` | admin | 200 `{seq}` |
| POST | `/admin/markets/{market}/pause` | admin | 200 `{seq, events}` |
| POST | `/admin/markets/{market}/resume` | admin | 200 `{seq, events}` |
| POST | `/admin/markets/{market}/close` | admin | 200 `{seq, events}` |
| POST | `/admin/markets/{market}/resolve` | admin | 200 `{seq, events}`, body `{"winner": "yes"}` |

`POST /orders` body:

```json
{"market": 1, "side": "buy", "outcome": "yes", "price": 6000, "qty": 10000000,
 "tif": "gtc", "post_only": false}
```

`tif` is `"gtc"` (default), `"fok"`, `"fak"`, or `{"gtd": {"expires_at": 1767225600000}}`. Send a market order as `fak` with a limit price.

Status codes:

- **400** `invalid_interval`, `invalid_status`.
- **401** `unauthorized`: missing or unknown key.
- **404** `unknown_market`, `unknown_order`, `not_owner`.
- **409** `market_exists`, `invalid_transition`.
- **422** rejected orders: `invalid_price`, `invalid_qty`, `invalid_time_in_force`, `already_expired`, `post_only_would_cross`, `fok_not_fillable`, `insufficient_funds`, `insufficient_shares`, `unknown_market`, `market_not_open`. Also `invalid_market_config`, `invalid_market_info` and `deposit_limit`.
- **503** `busy` (queue full), `unavailable` (exchange stopped), `history_disabled` or `history_unavailable`.

A response is sent only after its input is on disk (fsync).

A market's `info` holds `title` (required, at most 200 characters), `description` and `closes_at` (epoch milliseconds). It is display-only and does not affect trading; markets without it return `null`.

### Market status

| Status | New orders | Cancels | Can move to |
|--------|-----------|---------|-------------|
| `"open"` | accepted | allowed | paused, closed, resolved |
| `"paused"` | rejected | allowed | open, closed, resolved |
| `"closed"` | rejected | book is empty | resolved |
| `{"resolved": {"winner": "yes"}}` | rejected | book is empty | - |

`close` and `resolve` cancel every open order (reason `market_closed`). `resolve` then pays 1 USDC per winning share and clears the market's positions.

### History

The four history endpoints read from Postgres, which a background task keeps filled. They can lag the exchange briefly.

- `limit` defaults to 100, at most 1000.
- `before` pages backwards: pass the smallest `id` (trades, fills) or `order_id` (orders) from the previous page.
- Order `status`: `open`, `filled`, `cancelled`, `rejected`.
- Candle `interval`: `1m` (default), `5m`, `15m`, `1h`, `4h`, `1d`. A candle's `start` is in epoch milliseconds.
- Tables are created automatically when the server starts.

## WebSocket

Connect to `/ws` and send:

```json
{"op": "subscribe", "channel": "book", "market": 1}
{"op": "subscribe", "channel": "trades", "market": 1}
{"op": "unsubscribe", "channel": "book", "market": 1}
{"op": "auth", "key": "alice-key"}
```

Messages from the server:

- `{"channel": "book", "market", "seq", "bids", "asks"}`: the whole book, sent on subscribe and whenever it changes. Ignore messages with a lower `seq` than the last one you received.
- `{"channel": "trades", "market", "seq", "data": <trade>}`.
- `{"channel": "user", "seq", "data": <event>}`: events for the account's own orders, after `auth`.
- `{"error": "lagged"}`: the client fell too far behind; the connection is closed and the client must reconnect.

## Operations

- **Logs** go to stdout through `tracing`: recovery at startup, the listen address, journal failures and history projection failures.
- **Metrics** at `/metrics`, all prefixed `windrunner_`: `inputs_total`, `trades_total`, `batches_total` (one fsync per batch), `commit_seconds_total`, `queue_rejections_total`, `sequence`, `history_sequence`, `ws_connections`. The gap between `sequence` and `history_sequence` is how far the history database lags.
- **Shutdown**: SIGINT or SIGTERM stops accepting new requests. Open connections get 5 seconds to finish.

## Benchmarks

```sh
cargo bench -p windrunner-engine --bench matching
cargo bench -p windrunner-exchange --bench exchange
cargo run --release -p windrunner-server --example load -- 256 2000
```

The `load` example runs many concurrent clients straight against the exchange thread (no HTTP); each order waits for its fsync, as it does in the server.

Results on one Apple Silicon laptop, as a rough guide:

| Measured | Result |
|----------|--------|
| Engine alone, mixed place/cancel | about 12 million ops/s |
| Engine + ledger | about 2.3 million inputs/s |
| With the journal, fsync every 1024 inputs | about 160 thousand inputs/s |
| `load`, 256 clients | about 21 thousand orders/s, p50 11 ms, p99 21 ms |
| `load`, 8 clients | about 1 thousand orders/s, p50 8 ms |

The limit is fsync (about 8 ms per commit on that machine), not matching. Throughput grows with the number of concurrent clients because one fsync covers more orders.

## Tests

```sh
cargo test
```

Tests that need Postgres only run when `WINDRUNNER_TEST_DATABASE_URL` is set; without it they pass without doing anything. Each test creates its own schema in that database and leaves it behind, so point it at a throwaway database.

```sh
WINDRUNNER_TEST_DATABASE_URL=postgres://user@localhost/windrunner_test cargo test
```

## License

[MIT](LICENSE)
