use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use windrunner_engine::BookConfig;
use windrunner_exchange::{ExchangeError, Input};
use windrunner_history::History;
use windrunner_types::{
    AccountId, Command, Event, MarketId, MarketInfo, NewOrder, OrderId, Outcome, Price, Qty, Side,
    TimeInForce,
};

use crate::actor::{Handle, HandleError, book_json};
use crate::ws;

pub struct Auth {
    pub admin_key: String,
    pub api_keys: HashMap<String, AccountId>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub exchange: Handle,
    pub auth: Arc<Auth>,
    pub history: Option<History>,
}

pub fn router(exchange: Handle, auth: Auth, history: Option<History>) -> Router {
    let state = AppState {
        exchange,
        auth: Arc::new(auth),
        history,
    };
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/markets", get(list_markets))
        .route("/markets/{market}/book", get(get_book))
        .route("/markets/{market}/orders/{order_id}", delete(cancel_order))
        .route("/orders", get(list_orders).post(place_order))
        .route("/fills", get(list_fills))
        .route("/markets/{market}/trades", get(market_trades))
        .route("/markets/{market}/candles", get(market_candles))
        .route("/account", get(get_account))
        .route("/withdrawals", post(withdraw))
        .route("/admin/markets", post(create_market))
        .route("/admin/deposits", post(deposit))
        .route("/admin/markets/{market}/info", put(set_market_info))
        .route("/admin/markets/{market}/pause", post(pause_market))
        .route("/admin/markets/{market}/resume", post(resume_market))
        .route("/admin/markets/{market}/close", post(close_market))
        .route("/admin/markets/{market}/resolve", post(resolve_market))
        .route("/ws", get(ws::handler))
        .with_state(state)
}

pub(crate) struct ApiError {
    status: StatusCode,
    code: String,
}

impl ApiError {
    fn new(status: StatusCode, code: impl Into<String>) -> Self {
        ApiError {
            status,
            code: code.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

impl From<HandleError> for ApiError {
    fn from(e: HandleError) -> Self {
        let code = match e {
            HandleError::Busy => "busy",
            HandleError::Stopped => "unavailable",
        };
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code)
    }
}

impl From<ExchangeError> for ApiError {
    fn from(e: ExchangeError) -> Self {
        match e {
            ExchangeError::UnknownMarket => ApiError::new(StatusCode::NOT_FOUND, "unknown_market"),
            ExchangeError::InvalidTransition => {
                ApiError::new(StatusCode::CONFLICT, "invalid_transition")
            }
            ExchangeError::InvalidMarketInfo => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_market_info")
            }
            ExchangeError::MarketExists => ApiError::new(StatusCode::CONFLICT, "market_exists"),
            ExchangeError::InvalidMarketConfig => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_market_config")
            }
            ExchangeError::InsufficientFunds => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds")
            }
            ExchangeError::DepositLimit => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "deposit_limit")
            }
        }
    }
}

fn reason_code(reason: &impl Serialize) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn unauthorized() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized")
}

struct Account(AccountId);

impl FromRequestParts<AppState> for Account {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        bearer(parts)
            .and_then(|key| state.auth.api_keys.get(key))
            .map(|&account| Account(account))
            .ok_or_else(unauthorized)
    }
}

struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        match bearer(parts) {
            Some(key) if key == state.auth.admin_key => Ok(Admin),
            _ => Err(unauthorized()),
        }
    }
}

type ApiResult = Result<(StatusCode, Json<Value>), ApiError>;

async fn list_markets(State(state): State<AppState>) -> ApiResult {
    let markets = state
        .exchange
        .query(|exchange| {
            exchange
                .engine()
                .books()
                .map(|book| {
                    json!({
                        "market": book.market(),
                        "status": exchange.market_status(book.market()),
                        "info": exchange.market_info(book.market()),
                        "tick": book.config().tick,
                        "lot": book.config().lot,
                        "best_bid": book.best_bid(),
                        "best_ask": book.best_ask(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .await?;
    Ok((StatusCode::OK, Json(json!({ "markets": markets }))))
}

#[derive(Deserialize)]
struct DepthQuery {
    depth: Option<usize>,
}

async fn get_book(
    State(state): State<AppState>,
    Path(market): Path<u32>,
    Query(query): Query<DepthQuery>,
) -> ApiResult {
    let depth = query.depth.unwrap_or(usize::MAX);
    let book = state
        .exchange
        .query(move |exchange| {
            let book = exchange.engine().book(MarketId(market))?;
            Some(book_json(book, exchange.seq(), depth))
        })
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_market"))?;
    Ok((StatusCode::OK, Json(book)))
}

#[derive(Deserialize)]
struct PlaceOrder {
    market: MarketId,
    side: Side,
    outcome: Outcome,
    price: Price,
    qty: Qty,
    tif: Option<TimeInForce>,
    #[serde(default)]
    post_only: bool,
}

async fn place_order(
    State(state): State<AppState>,
    Account(account): Account,
    Json(req): Json<PlaceOrder>,
) -> ApiResult {
    let order = NewOrder {
        id: OrderId(0),
        market: req.market,
        account,
        side: req.side,
        outcome: req.outcome,
        price: req.price,
        qty: req.qty,
        tif: req.tif.unwrap_or(TimeInForce::Gtc),
        post_only: req.post_only,
    };
    let done = state
        .exchange
        .submit(Input::Order(Command::Place(order)))
        .await?;
    let events = done.result?;
    let rejection = events.iter().find_map(|e| match e {
        Event::Rejected { reason, .. } => Some(reason),
        _ => None,
    });
    if let Some(reason) = rejection {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            reason_code(reason),
        ));
    }
    let body = json!({ "order_id": done.order_id, "seq": done.seq, "events": events });
    Ok((StatusCode::CREATED, Json(body)))
}

async fn cancel_order(
    State(state): State<AppState>,
    Account(account): Account,
    Path((market, order_id)): Path<(u32, u64)>,
) -> ApiResult {
    let cancel = Command::Cancel {
        market: MarketId(market),
        order_id: OrderId(order_id),
        account,
    };
    let done = state.exchange.submit(Input::Order(cancel)).await?;
    let events = done.result?;
    let rejection = events.iter().find_map(|e| match e {
        Event::CancelRejected { reason, .. } => Some(reason),
        _ => None,
    });
    if let Some(reason) = rejection {
        return Err(ApiError::new(StatusCode::NOT_FOUND, reason_code(reason)));
    }
    let body = json!({ "seq": done.seq, "events": events });
    Ok((StatusCode::OK, Json(body)))
}

async fn get_account(State(state): State<AppState>, Account(account): Account) -> ApiResult {
    let body = state
        .exchange
        .query(move |exchange| {
            let ledger = exchange.ledger();
            let positions: Vec<Value> = ledger
                .positions_of(account)
                .into_iter()
                .map(|(market, outcome, balance)| {
                    json!({
                        "market": market,
                        "outcome": outcome,
                        "available": balance.available,
                        "reserved": balance.reserved,
                    })
                })
                .collect();
            json!({
                "account": account,
                "usdc": ledger.usdc(account),
                "positions": positions,
                "open_orders": ledger.open_orders(account),
            })
        })
        .await?;
    Ok((StatusCode::OK, Json(body)))
}

#[derive(Deserialize)]
struct Withdraw {
    amount: u64,
}

async fn withdraw(
    State(state): State<AppState>,
    Account(account): Account,
    Json(req): Json<Withdraw>,
) -> ApiResult {
    let input = Input::Withdraw {
        account,
        amount: req.amount,
    };
    let done = state.exchange.submit(input).await?;
    done.result?;
    Ok((StatusCode::OK, Json(json!({ "seq": done.seq }))))
}

#[derive(Deserialize)]
struct CreateMarket {
    market: MarketId,
    tick: Option<u16>,
    lot: Option<u64>,
    info: Option<MarketInfo>,
}

async fn create_market(
    State(state): State<AppState>,
    _: Admin,
    Json(req): Json<CreateMarket>,
) -> ApiResult {
    let default = BookConfig::default();
    let config = BookConfig {
        tick: req.tick.unwrap_or(default.tick),
        lot: req.lot.unwrap_or(default.lot),
        ..default
    };
    let input = Input::CreateMarket {
        market: req.market,
        config,
    };
    // Checked up front so a bad description cannot leave a market behind
    // that was created but never described.
    if req.info.as_ref().is_some_and(|info| !info.is_valid()) {
        return Err(ExchangeError::InvalidMarketInfo.into());
    }
    let done = state.exchange.submit(input).await?;
    done.result?;
    if let Some(info) = req.info {
        let market = req.market;
        let done = state
            .exchange
            .submit(Input::SetMarketInfo { market, info })
            .await?;
        done.result?;
    }
    let body = json!({ "market": req.market, "tick": config.tick, "lot": config.lot });
    Ok((StatusCode::CREATED, Json(body)))
}

#[derive(Deserialize)]
struct Deposit {
    account: AccountId,
    amount: u64,
}

async fn deposit(State(state): State<AppState>, _: Admin, Json(req): Json<Deposit>) -> ApiResult {
    let input = Input::Deposit {
        account: req.account,
        amount: req.amount,
    };
    let done = state.exchange.submit(input).await?;
    done.result?;
    Ok((StatusCode::OK, Json(json!({ "seq": done.seq }))))
}

async fn lifecycle(state: &AppState, input: Input) -> ApiResult {
    let done = state.exchange.submit(input).await?;
    let events = done.result?;
    let body = json!({ "seq": done.seq, "events": events });
    Ok((StatusCode::OK, Json(body)))
}

async fn pause_market(
    State(state): State<AppState>,
    _: Admin,
    Path(market): Path<u32>,
) -> ApiResult {
    let market = MarketId(market);
    lifecycle(&state, Input::PauseMarket { market }).await
}

async fn resume_market(
    State(state): State<AppState>,
    _: Admin,
    Path(market): Path<u32>,
) -> ApiResult {
    let market = MarketId(market);
    lifecycle(&state, Input::ResumeMarket { market }).await
}

async fn close_market(
    State(state): State<AppState>,
    _: Admin,
    Path(market): Path<u32>,
) -> ApiResult {
    let market = MarketId(market);
    lifecycle(&state, Input::CloseMarket { market }).await
}

#[derive(Deserialize)]
struct Resolve {
    winner: Outcome,
}

async fn resolve_market(
    State(state): State<AppState>,
    _: Admin,
    Path(market): Path<u32>,
    Json(req): Json<Resolve>,
) -> ApiResult {
    let input = Input::ResolveMarket {
        market: MarketId(market),
        winner: req.winner,
    };
    lifecycle(&state, input).await
}

impl From<windrunner_history::Error> for ApiError {
    fn from(e: windrunner_history::Error) -> Self {
        tracing::warn!(error = %e, "history query failed");
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "history_unavailable")
    }
}

fn history(state: &AppState) -> Result<&History, ApiError> {
    state
        .history
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "history_disabled"))
}

#[derive(Deserialize)]
struct Page {
    before: Option<i64>,
    limit: Option<i64>,
    status: Option<String>,
    interval: Option<String>,
}

impl Page {
    fn before(&self) -> i64 {
        self.before.unwrap_or(i64::MAX)
    }

    fn limit(&self) -> i64 {
        self.limit.unwrap_or(100).clamp(1, 1000)
    }
}

fn bad_request(code: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code)
}

async fn market_trades(
    State(state): State<AppState>,
    Path(market): Path<u32>,
    Query(page): Query<Page>,
) -> ApiResult {
    let trades = history(&state)?
        .market_trades(market, page.before(), page.limit())
        .await?;
    Ok((StatusCode::OK, Json(json!({ "trades": trades }))))
}

async fn market_candles(
    State(state): State<AppState>,
    Path(market): Path<u32>,
    Query(page): Query<Page>,
) -> ApiResult {
    let minutes = match page.interval.as_deref().unwrap_or("1m") {
        "1m" => 1,
        "5m" => 5,
        "15m" => 15,
        "1h" => 60,
        "4h" => 240,
        "1d" => 1440,
        _ => return Err(bad_request("invalid_interval")),
    };
    let candles = history(&state)?
        .candles(market, minutes * 60_000, page.limit())
        .await?;
    Ok((StatusCode::OK, Json(json!({ "candles": candles }))))
}

async fn list_orders(
    State(state): State<AppState>,
    Account(account): Account,
    Query(page): Query<Page>,
) -> ApiResult {
    let status = page.status.as_deref();
    if !matches!(
        status,
        None | Some("open" | "filled" | "cancelled" | "rejected")
    ) {
        return Err(bad_request("invalid_status"));
    }
    let orders = history(&state)?
        .account_orders(account.0, status, page.before(), page.limit())
        .await?;
    Ok((StatusCode::OK, Json(json!({ "orders": orders }))))
}

async fn list_fills(
    State(state): State<AppState>,
    Account(account): Account,
    Query(page): Query<Page>,
) -> ApiResult {
    let fills = history(&state)?
        .account_fills(account.0, page.before(), page.limit())
        .await?;
    Ok((StatusCode::OK, Json(json!({ "fills": fills }))))
}

async fn metrics(State(state): State<AppState>) -> String {
    state.exchange.metrics().render()
}

async fn set_market_info(
    State(state): State<AppState>,
    _: Admin,
    Path(market): Path<u32>,
    Json(info): Json<MarketInfo>,
) -> ApiResult {
    let market = MarketId(market);
    lifecycle(&state, Input::SetMarketInfo { market, info }).await
}
