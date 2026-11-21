use std::collections::HashSet;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use windrunner_types::MarketId;

use std::sync::atomic::Ordering;

use crate::actor::{Handle, Topic, book_json};
use crate::app::AppState;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Channel {
    Book,
    Trades,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientMessage {
    Subscribe { channel: Channel, market: MarketId },
    Unsubscribe { channel: Channel, market: MarketId },
    Auth { key: String },
}

fn topic(channel: Channel, market: MarketId) -> Topic {
    match channel {
        Channel::Book => Topic::Book(market),
        Channel::Trades => Topic::Trades(market),
    }
}

pub(crate) async fn handler(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    upgrade.on_upgrade(move |socket| serve(socket, state))
}

async fn serve(mut socket: WebSocket, state: AppState) {
    let _connected = Connected::new(state.exchange.clone());
    let mut feed = state.exchange.subscribe();
    let mut topics = HashSet::new();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                };
                for reply in on_client_message(&state, &mut topics, text.as_str()).await {
                    if socket.send(Message::Text(reply.into())).await.is_err() {
                        return;
                    }
                }
            }
            item = feed.recv() => match item {
                Ok(item) => {
                    if topics.contains(&item.topic)
                        && socket.send(Message::Text(item.json.as_ref().into())).await.is_err()
                    {
                        return;
                    }
                }
                // The client missed messages it cannot recover from this
                // stream; it has to reconnect and resubscribe.
                Err(RecvError::Lagged(_)) => {
                    let _ = socket.send(Message::Text(r#"{"error":"lagged"}"#.into())).await;
                    return;
                }
                Err(RecvError::Closed) => return,
            }
        }
    }
}

async fn on_client_message(
    state: &AppState,
    topics: &mut HashSet<Topic>,
    text: &str,
) -> Vec<String> {
    let error = |code: &str| vec![json!({ "error": code }).to_string()];
    let Ok(message) = serde_json::from_str::<ClientMessage>(text) else {
        return error("bad_request");
    };
    match message {
        ClientMessage::Auth { key } => match state.auth.api_keys.get(&key) {
            Some(&account) => {
                topics.insert(Topic::User(account));
                vec![json!({ "op": "authenticated", "account": account }).to_string()]
            }
            None => error("unauthorized"),
        },
        ClientMessage::Unsubscribe { channel, market } => {
            topics.remove(&topic(channel, market));
            vec![json!({ "op": "unsubscribed", "market": market }).to_string()]
        }
        ClientMessage::Subscribe { channel, market } => {
            // Subscribe before reading the snapshot so no update can fall
            // between the two; `seq` lets the client discard stale ones.
            let snapshot = state
                .exchange
                .query(move |exchange| {
                    let book = exchange.engine().book(market)?;
                    Some(book_json(book, exchange.seq(), usize::MAX))
                })
                .await;
            let Ok(Some(mut snapshot)) = snapshot else {
                return error(if snapshot.is_ok() {
                    "unknown_market"
                } else {
                    "unavailable"
                });
            };
            topics.insert(topic(channel, market));
            let mut replies = vec![json!({ "op": "subscribed", "market": market }).to_string()];
            if matches!(channel, Channel::Book) {
                snapshot["channel"] = "book".into();
                replies.push(snapshot.to_string());
            }
            replies
        }
    }
}

struct Connected(Handle);

impl Connected {
    fn new(handle: Handle) -> Self {
        handle
            .metrics()
            .ws_connections
            .fetch_add(1, Ordering::Relaxed);
        Connected(handle)
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.0
            .metrics()
            .ws_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}
