# windrunner

Matching engine untuk prediction market biner (YES/NO), ditulis dengan Rust.

## Struktur

| Crate | Isi |
|-------|-----|
| `crates/types` | Tipe dasar: `Price`, `Qty`, order, command, event |
| `crates/engine` | Order book dan matching, tanpa IO |
| `crates/ledger` | Saldo, reservasi, settlement mint/merge/transfer, payout |
| `crates/journal` | WAL dan snapshot di disk |
| `crates/exchange` | `Exchange` (engine + ledger) dan `Sequencer` (log lalu apply) |
| `crates/history` | Proyeksi ke Postgres: history order, trade, dan candle |
| `crates/server` | HTTP + WebSocket (axum) |

## Menjalankan

```sh
WINDRUNNER_ADMIN_KEY=rahasia \
WINDRUNNER_API_KEYS=kunci-alice:1,kunci-bob:2 \
cargo run -p windrunner-server
```

| Variabel | Default | Arti |
|----------|---------|------|
| `WINDRUNNER_ADDR` | `127.0.0.1:8080` | Alamat listen |
| `WINDRUNNER_DATA_DIR` | `data` | Folder WAL dan snapshot |
| `WINDRUNNER_ADMIN_KEY` | wajib | Kunci untuk endpoint `/admin` |
| `WINDRUNNER_API_KEYS` | kosong | Daftar `kunci:account_id`, dipisah koma |
| `WINDRUNNER_CORS_ORIGINS` | tidak ada | Origin browser yang diizinkan, dipisah koma, atau `*`. Tanpa ini tidak ada header CORS |
| `WINDRUNNER_DATABASE_URL` | tidak ada | URL Postgres untuk history, misalnya `postgres://user@localhost/windrunner`. Tanpa ini endpoint history menjawab 503 `history_disabled` |

## Satuan

Semua angka di API adalah integer.

- **Harga**: basis poin dari 1 USDC. `6000` berarti 0.60. Harga order adalah harga outcome-nya sendiri, jadi Buy NO @ 4000 berarti membayar 0.40 per share NO.
- **Qty**: micro-share. `1000000` berarti 1 share.
- **USDC**: micro-USDC. `1000000` berarti 1 USDC.
- **Waktu** (`expires_at`): milidetik sejak Unix epoch.
- **Order book** selalu dalam harga YES: `bids` dan `asks` berisi pasangan `[harga, qty]`, terbaik dulu.

## REST

Autentikasi memakai header `Authorization: Bearer <kunci>`. Error selalu berbentuk `{"error": "<kode>"}`.

| Method | Path | Auth | Hasil |
|--------|------|------|-------|
| GET | `/health` | - | `ok` |
| GET | `/metrics` | - | Metrics format Prometheus |
| GET | `/markets` | - | Daftar market dengan status, `info`, tick, lot, best bid/ask |
| GET | `/markets/{market}/book?depth=N` | - | `{market, seq, bids, asks}` |
| POST | `/orders` | user | 201 `{order_id, seq, events}` |
| DELETE | `/markets/{market}/orders/{order_id}` | user | 200 `{seq, events}` |
| GET | `/account` | user | `{account, usdc, positions, open_orders}` |
| POST | `/withdrawals` | user | 200 `{seq}` |
| GET | `/orders?status=&before=&limit=` | user | `{orders}`: history order akun, terbaru dulu |
| GET | `/fills?before=&limit=` | user | `{fills}`: trade milik akun, dengan `role` maker/taker |
| GET | `/markets/{market}/trades?before=&limit=` | - | `{trades}`: trade publik tanpa identitas akun |
| GET | `/markets/{market}/candles?interval=&limit=` | - | `{candles}`: OHLC harga YES, terbaru dulu |
| POST | `/admin/markets` | admin | 201 `{market, tick, lot}`; body boleh memuat `info` |
| PUT | `/admin/markets/{market}/info` | admin | 200 `{seq, events}`, body `{title, description, closes_at}` |
| POST | `/admin/deposits` | admin | 200 `{seq}` |
| POST | `/admin/markets/{market}/pause` | admin | 200 `{seq, events}` |
| POST | `/admin/markets/{market}/resume` | admin | 200 `{seq, events}` |
| POST | `/admin/markets/{market}/close` | admin | 200 `{seq, events}` |
| POST | `/admin/markets/{market}/resolve` | admin | 200 `{seq, events}`, body `{"winner": "yes"}` |

Body `POST /orders`:

```json
{"market": 1, "side": "buy", "outcome": "yes", "price": 6000, "qty": 10000000,
 "tif": "gtc", "post_only": false}
```

`tif` boleh `"gtc"` (default), `"fok"`, `"fak"`, atau `{"gtd": {"expires_at": 1767225600000}}`. Market order dikirim sebagai `fak` dengan harga batas.

Kode status:

- **401** `unauthorized`: kunci salah atau tidak ada.
- **404** `unknown_market`, `unknown_order`, `not_owner`.
- **409** `market_exists`, `invalid_transition`.
- **422** order ditolak: `invalid_price`, `invalid_qty`, `invalid_time_in_force`, `already_expired`, `post_only_would_cross`, `fok_not_fillable`, `insufficient_funds`, `insufficient_shares`, `unknown_market`, `market_not_open`. Juga `invalid_market_config`, `invalid_market_info`, dan `deposit_limit`.
- **400** `invalid_interval`, `invalid_status`.
- **503** `busy` (antrean penuh), `unavailable` (exchange berhenti), `history_disabled`, atau `history_unavailable`.

Respons baru dikirim setelah input tersimpan di disk (fsync).

`info` market berisi `title` (wajib, maksimum 200 karakter), `description`, dan `closes_at` (milidetik epoch). Isinya hanya untuk tampilan dan tidak memengaruhi trading; market tanpa `info` menampilkan `null`.

### Status market

| Status | Order baru | Cancel | Bisa pindah ke |
|--------|-----------|--------|----------------|
| `"open"` | ya | ya | paused, closed, resolved |
| `"paused"` | ditolak | ya | open, closed, resolved |
| `"closed"` | ditolak | buku kosong | resolved |
| `{"resolved": {"winner": "yes"}}` | ditolak | buku kosong | - |

`close` dan `resolve` membatalkan semua order terbuka (alasan `market_closed`). `resolve` lalu membayar 1 USDC per share yang menang dan menghapus posisi market itu.

### History

Empat endpoint history membaca dari Postgres, yang diisi oleh proses latar belakang. Datanya bisa tertinggal sesaat dari exchange.

- `limit` default 100, maksimum 1000.
- `before` untuk halaman berikutnya: isi dengan `id` (trade, fill) atau `order_id` (order) terkecil dari halaman sebelumnya.
- `status` order: `open`, `filled`, `cancelled`, `rejected`.
- `interval` candle: `1m` (default), `5m`, `15m`, `1h`, `4h`, `1d`. `start` candle dalam milidetik epoch.
- Tabel dibuat otomatis saat server start.

## WebSocket

Sambung ke `/ws`, lalu kirim:

```json
{"op": "subscribe", "channel": "book", "market": 1}
{"op": "subscribe", "channel": "trades", "market": 1}
{"op": "unsubscribe", "channel": "book", "market": 1}
{"op": "auth", "key": "kunci-alice"}
```

Pesan dari server:

- `{"channel": "book", "market", "seq", "bids", "asks"}`: seluruh buku, dikirim saat subscribe dan setiap kali buku berubah. Abaikan pesan dengan `seq` lebih kecil dari yang terakhir diterima.
- `{"channel": "trades", "market", "seq", "data": <trade>}`.
- `{"channel": "user", "seq", "data": <event>}`: event order milik akun, aktif setelah `auth`.
- `{"error": "lagged"}`: klien terlalu lambat; koneksi ditutup dan klien harus menyambung ulang.

## Operasional

- **Log** ditulis ke stdout lewat `tracing`: recovery saat start, alamat listen, kegagalan journal, dan kegagalan proyeksi history.
- **Metrics** di `/metrics`, semua berawalan `windrunner_`: `inputs_total`, `trades_total`, `batches_total` (satu fsync per batch), `commit_seconds_total`, `queue_rejections_total`, `sequence`, `history_sequence`, `ws_connections`. Selisih `sequence` dan `history_sequence` adalah ketertinggalan database history.
- **Shutdown**: SIGINT atau SIGTERM menghentikan penerimaan request baru. Koneksi yang masih terbuka diberi waktu 5 detik.

## Benchmark

```sh
cargo bench -p windrunner-engine --bench matching
cargo bench -p windrunner-exchange --bench exchange
cargo run --release -p windrunner-server --example load -- 256 2000
```

Contoh `load` menjalankan banyak klien serentak langsung ke thread exchange (tanpa HTTP); tiap order menunggu fsync seperti di server.

Hasil di satu laptop Apple Silicon, sekadar gambaran:

| Yang diukur | Hasil |
|-------------|-------|
| Engine saja, campuran place/cancel | sekitar 12 juta operasi/detik |
| Engine + ledger | sekitar 2,3 juta input/detik |
| Dengan journal, fsync tiap 1024 input | sekitar 160 ribu input/detik |
| `load`, 256 klien | sekitar 21 ribu order/detik, p50 11 ms, p99 21 ms |
| `load`, 8 klien | sekitar 1 ribu order/detik, p50 8 ms |

Batasnya adalah fsync (sekitar 8 ms per commit di mesin itu), bukan matching. Throughput naik seiring jumlah klien serentak karena satu fsync menanggung lebih banyak order.

## Test

```sh
cargo test
```

Test yang butuh Postgres hanya berjalan kalau `WINDRUNNER_TEST_DATABASE_URL` diisi; tanpa itu test tersebut lulus tanpa melakukan apa-apa. Tiap test membuat schema sendiri di database itu dan tidak menghapusnya, jadi pakai database sekali pakai.

```sh
WINDRUNNER_TEST_DATABASE_URL=postgres://user@localhost/windrunner_test cargo test
```

## Lisensi

[MIT](LICENSE)
