use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;
use windrunner_server::actor::{self, Config, Projection};
use windrunner_server::{Auth, History, router, start_projection};
use windrunner_types::AccountId;

const USDC: u64 = 1_000_000;
const SHARE: u64 = 1_000_000;

fn app(dir: &Path) -> Router {
    app_with(dir, None, None)
}

fn app_with(dir: &Path, history: Option<History>, projection: Option<Projection>) -> Router {
    let exchange = actor::spawn(dir, Config::default(), projection).unwrap();
    let api_keys = HashMap::from([
        ("alice".to_owned(), AccountId(1)),
        ("bob".to_owned(), AccountId(2)),
    ]);
    router(
        exchange,
        Auth {
            admin_key: "admin".into(),
            api_keys,
        },
        history,
    )
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn seed(app: &Router) {
    let market = json!({ "market": 1 });
    let (status, _) = call(app, "POST", "/admin/markets", Some("admin"), Some(market)).await;
    assert_eq!(status, StatusCode::CREATED);
    for account in [1, 2] {
        let deposit = json!({ "account": account, "amount": 100 * USDC });
        let (status, _) = call(app, "POST", "/admin/deposits", Some("admin"), Some(deposit)).await;
        assert_eq!(status, StatusCode::OK);
    }
}

fn order(side: &str, outcome: &str, price: u64, shares: u64) -> Option<Value> {
    Some(json!({
        "market": 1,
        "side": side,
        "outcome": outcome,
        "price": price,
        "qty": shares * SHARE,
    }))
}

#[tokio::test]
async fn auth_is_required() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());

    let (status, body) = call(&app, "POST", "/orders", None, order("buy", "yes", 6000, 1)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error": "unauthorized" }));

    let (status, _) = call(&app, "GET", "/account", Some("mallory"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let market = Some(json!({ "market": 1 }));
    let (status, _) = call(&app, "POST", "/admin/markets", Some("alice"), market).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call(&app, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn trading_flow() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    seed(&app).await;

    let market = Some(json!({ "market": 1 }));
    let (status, body) = call(&app, "POST", "/admin/markets", Some("admin"), market).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "market_exists");

    let (status, body) = call(
        &app,
        "POST",
        "/orders",
        Some("alice"),
        order("buy", "yes", 6000, 10),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let alice_order = body["order_id"].as_u64().unwrap();
    assert_eq!(
        body["events"][0],
        json!({ "accepted": { "order_id": alice_order } })
    );
    assert_eq!(body["events"][1]["rested"]["remaining"], 10 * SHARE);

    let (status, book) = call(&app, "GET", "/markets/1/book", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(book["bids"], json!([[6000, 10 * SHARE]]));
    assert_eq!(book["asks"], json!([]));

    let (status, _) = call(&app, "GET", "/markets/9/book", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 4),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let trade = &body["events"][1]["trade"];
    assert_eq!(trade["kind"], "mint");
    assert_eq!(trade["yes_price"], 6000);
    assert_eq!(trade["qty"], 4 * SHARE);
    assert_eq!(trade["maker"]["order_id"], alice_order);

    let (_, account) = call(&app, "GET", "/account", Some("alice"), None).await;
    assert_eq!(
        account["usdc"],
        json!({ "available": 94 * USDC, "reserved": 3_600_000 })
    );
    assert_eq!(
        account["positions"],
        json!([{ "market": 1, "outcome": "yes", "available": 4 * SHARE, "reserved": 0 }])
    );
    assert_eq!(account["open_orders"][0]["order_id"], alice_order);
    assert_eq!(account["open_orders"][0]["remaining"], 6 * SHARE);

    let (status, body) = call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "yes", 5000, 500),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "insufficient_funds");

    let (status, body) = call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "yes", 5050, 1),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "invalid_price");

    let path = format!("/markets/1/orders/{alice_order}");
    let (status, body) = call(&app, "DELETE", &path, Some("bob"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not_owner");

    let (status, body) = call(&app, "DELETE", &path, Some("alice"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"][0]["cancelled"]["reason"], "user");

    let (_, account) = call(&app, "GET", "/account", Some("alice"), None).await;
    assert_eq!(
        account["usdc"],
        json!({ "available": 97_600_000, "reserved": 0 })
    );
    assert_eq!(account["open_orders"], json!([]));

    let (_, markets) = call(&app, "GET", "/markets", None, None).await;
    assert_eq!(
        markets["markets"],
        json!([{ "market": 1, "status": "open", "info": null, "tick": 100, "lot": 10_000, "best_bid": null, "best_ask": null }])
    );

    let withdraw = Some(json!({ "amount": 98 * USDC }));
    let (status, body) = call(&app, "POST", "/withdrawals", Some("alice"), withdraw).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "insufficient_funds");
}

#[tokio::test]
async fn state_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = app(dir.path());
    seed(&first).await;
    call(
        &first,
        "POST",
        "/orders",
        Some("alice"),
        order("buy", "yes", 6000, 10),
    )
    .await;
    call(
        &first,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 4),
    )
    .await;
    let (_, before) = call(&first, "GET", "/account", Some("alice"), None).await;
    let (_, book_before) = call(&first, "GET", "/markets/1/book", None, None).await;
    drop(first);

    let second = app(dir.path());
    let (_, after) = call(&second, "GET", "/account", Some("alice"), None).await;
    let (_, book_after) = call(&second, "GET", "/markets/1/book", None, None).await;
    assert_eq!(after, before);
    assert_eq!(book_after["bids"], book_before["bids"]);

    let (status, body) = call(
        &second,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 1),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        body["order_id"].as_u64().unwrap() > before["open_orders"][0]["order_id"].as_u64().unwrap()
    );
}

#[tokio::test]
async fn websocket_streams_book_trades_and_user_events() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    seed(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    macro_rules! next {
        () => {{
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("timed out waiting for a websocket message")
                .unwrap()
                .unwrap();
            serde_json::from_str::<Value>(message.to_text().unwrap()).unwrap()
        }};
    }
    macro_rules! send {
        ($value:expr) => {
            socket
                .send(Message::text($value.to_string()))
                .await
                .unwrap()
        };
    }

    send!(json!({ "op": "subscribe", "channel": "book", "market": 9 }));
    assert_eq!(next!(), json!({ "error": "unknown_market" }));
    send!(json!({ "op": "auth", "key": "nope" }));
    assert_eq!(next!(), json!({ "error": "unauthorized" }));

    send!(json!({ "op": "subscribe", "channel": "book", "market": 1 }));
    assert_eq!(next!(), json!({ "op": "subscribed", "market": 1 }));
    let snapshot = next!();
    assert_eq!(snapshot["channel"], "book");
    assert_eq!(snapshot["bids"], json!([]));
    send!(json!({ "op": "subscribe", "channel": "trades", "market": 1 }));
    assert_eq!(next!(), json!({ "op": "subscribed", "market": 1 }));
    send!(json!({ "op": "auth", "key": "alice" }));
    assert_eq!(next!(), json!({ "op": "authenticated", "account": 1 }));

    call(
        &app,
        "POST",
        "/orders",
        Some("alice"),
        order("buy", "yes", 6000, 10),
    )
    .await;
    let accepted = next!();
    assert_eq!(accepted["channel"], "user");
    assert!(accepted["data"]["accepted"].is_object());
    assert!(next!()["data"]["rested"].is_object());
    let book = next!();
    assert_eq!(book["channel"], "book");
    assert_eq!(book["bids"], json!([[6000, 10 * SHARE]]));
    assert!(book["seq"].as_u64() > snapshot["seq"].as_u64());

    call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 10),
    )
    .await;
    let fill = next!();
    assert_eq!(fill["channel"], "user");
    assert_eq!(fill["data"]["trade"]["kind"], "mint");
    let trade = next!();
    assert_eq!(trade["channel"], "trades");
    assert_eq!(trade["data"]["qty"], 10 * SHARE);
    let book = next!();
    assert_eq!(book["bids"], json!([]));
}

#[tokio::test]
async fn market_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    seed(&app).await;
    let admin = Some("admin");

    call(
        &app,
        "POST",
        "/orders",
        Some("alice"),
        order("buy", "yes", 6000, 10),
    )
    .await;
    call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 4),
    )
    .await;

    let (status, _) = call(&app, "POST", "/admin/markets/1/pause", Some("alice"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&app, "POST", "/admin/markets/9/pause", admin, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_market");

    let (status, _) = call(&app, "POST", "/admin/markets/1/pause", admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 1),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "market_not_open");
    let (_, markets) = call(&app, "GET", "/markets", None, None).await;
    assert_eq!(markets["markets"][0]["status"], "paused");

    let (status, _) = call(&app, "POST", "/admin/markets/1/resume", admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 1),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let winner = Some(json!({ "winner": "yes" }));
    let (status, body) = call(
        &app,
        "POST",
        "/admin/markets/1/resolve",
        admin,
        winner.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"][0]["cancelled"]["reason"], "market_closed");
    assert_eq!(body["events"][0]["cancelled"]["remaining"], 5 * SHARE);

    let (_, alice) = call(&app, "GET", "/account", Some("alice"), None).await;
    assert_eq!(
        alice["usdc"],
        json!({ "available": 102 * USDC, "reserved": 0 })
    );
    assert_eq!(alice["positions"], json!([]));
    assert_eq!(alice["open_orders"], json!([]));
    let (_, bob) = call(&app, "GET", "/account", Some("bob"), None).await;
    assert_eq!(
        bob["usdc"],
        json!({ "available": 98 * USDC, "reserved": 0 })
    );

    let (_, markets) = call(&app, "GET", "/markets", None, None).await;
    assert_eq!(
        markets["markets"][0]["status"],
        json!({ "resolved": { "winner": "yes" } })
    );
    let (status, body) = call(&app, "POST", "/admin/markets/1/resolve", admin, winner).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "invalid_transition");
}

async fn test_history() -> Option<History> {
    let url = std::env::var("WINDRUNNER_TEST_DATABASE_URL").ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("api_{}_{nanos}", std::process::id());
    Some(History::connect_schema(&url, &schema).await.unwrap())
}

async fn eventually(
    app: &Router,
    path: &str,
    key: Option<&str>,
    done: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..100 {
        let (status, body) = call(app, "GET", path, key, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if done(&body) {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{path} never reached the expected state");
}

#[tokio::test]
async fn history_endpoints_need_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    for (path, key) in [
        ("/markets/1/trades", None),
        ("/markets/1/candles", None),
        ("/orders", Some("alice")),
        ("/fills", Some("alice")),
    ] {
        let (status, body) = call(&app, "GET", path, key, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "history_disabled");
    }
    let (status, _) = call(&app, "GET", "/orders", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn history_is_served_and_catches_up_after_restart() {
    let Some(history) = test_history().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    // First run without a database: these inputs only exist in the journal.
    let first = app(dir.path());
    seed(&first).await;
    call(
        &first,
        "POST",
        "/orders",
        Some("alice"),
        order("buy", "yes", 6000, 10),
    )
    .await;
    call(
        &first,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 4),
    )
    .await;
    drop(first);

    let projection = start_projection(&history).await.unwrap();
    let app = app_with(dir.path(), Some(history), Some(projection));
    let body = eventually(&app, "/markets/1/trades", None, |b| {
        b["trades"].as_array().unwrap().len() == 1
    })
    .await;
    assert_eq!(body["trades"][0]["kind"], "mint");
    assert_eq!(body["trades"][0]["qty"], 4 * SHARE);
    assert!(body["trades"][0].get("maker_account").is_none());

    call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 6),
    )
    .await;
    let body = eventually(&app, "/markets/1/trades", None, |b| {
        b["trades"].as_array().unwrap().len() == 2
    })
    .await;
    let newest = body["trades"][0]["id"].as_i64().unwrap();
    let (_, older) = call(
        &app,
        "GET",
        &format!("/markets/1/trades?before={newest}&limit=5"),
        None,
        None,
    )
    .await;
    assert_eq!(older["trades"], json!([body["trades"][1]]));

    let orders = eventually(&app, "/orders?status=filled", Some("alice"), |b| {
        b["orders"].as_array().unwrap().len() == 1
    })
    .await;
    assert_eq!(orders["orders"][0]["filled"], 10 * SHARE);
    let (_, fills) = call(&app, "GET", "/fills", Some("alice"), None).await;
    assert_eq!(fills["fills"].as_array().unwrap().len(), 2);
    assert_eq!(fills["fills"][0]["role"], "maker");
    let (_, fills) = call(&app, "GET", "/fills", Some("bob"), None).await;
    assert_eq!(fills["fills"][0]["role"], "taker");

    let (_, candles) = call(&app, "GET", "/markets/1/candles?interval=1d", None, None).await;
    let candle = &candles["candles"][0];
    assert_eq!(
        (candle["open"].as_i64(), candle["close"].as_i64()),
        (Some(6000), Some(6000))
    );
    assert_eq!(candle["volume"], 10 * SHARE);
    assert_eq!(candle["trades"], 2);

    let (status, body) = call(&app, "GET", "/markets/1/candles?interval=7m", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_interval");
    let (status, body) = call(&app, "GET", "/orders?status=weird", Some("alice"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_status");
}

#[tokio::test]
async fn metrics_count_inputs_and_trades() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    seed(&app).await;
    call(
        &app,
        "POST",
        "/orders",
        Some("alice"),
        order("buy", "yes", 6000, 10),
    )
    .await;
    call(
        &app,
        "POST",
        "/orders",
        Some("bob"),
        order("buy", "no", 4000, 4),
    )
    .await;

    let request = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let value = |name: &str| -> f64 {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("windrunner_{name} ")))
            .unwrap();
        line.rsplit(' ').next().unwrap().parse().unwrap()
    };
    assert_eq!(value("trades_total"), 1.0);
    assert!(value("inputs_total") >= 5.0);
    assert!(value("sequence") >= 5.0);
    assert!(value("batches_total") >= 1.0);
    assert_eq!(value("ws_connections"), 0.0);
    assert!(text.contains("# TYPE windrunner_inputs_total counter"));
}

#[tokio::test]
async fn markets_carry_a_description() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let admin = Some("admin");

    let blank = json!({ "market": 1, "info": { "title": "  " } });
    let (status, body) = call(&app, "POST", "/admin/markets", admin, Some(blank)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "invalid_market_info");
    let (_, markets) = call(&app, "GET", "/markets", None, None).await;
    assert_eq!(markets["markets"], json!([]));

    let info = json!({ "title": "Will it rain tomorrow?", "description": "Resolves on the forecast.", "closes_at": 1_800_000_000_000u64 });
    let create = json!({ "market": 1, "info": info });
    let (status, _) = call(&app, "POST", "/admin/markets", admin, Some(create)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, markets) = call(&app, "GET", "/markets", None, None).await;
    assert_eq!(markets["markets"][0]["info"], info);

    let renamed = json!({ "title": "Will it rain?" });
    let (status, _) = call(
        &app,
        "PUT",
        "/admin/markets/1/info",
        admin,
        Some(renamed.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "PUT",
        "/admin/markets/1/info",
        Some("alice"),
        Some(renamed.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&app, "PUT", "/admin/markets/9/info", admin, Some(renamed)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_market");
    drop(app);

    let app = app_with(dir.path(), None, None);
    let (_, markets) = call(&app, "GET", "/markets", None, None).await;
    assert_eq!(
        markets["markets"][0]["info"],
        json!({ "title": "Will it rain?", "description": "", "closes_at": null })
    );
}
