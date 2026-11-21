use std::collections::HashMap;
use std::env;
use std::error::Error;

use std::time::Duration;

use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderValue, Method};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::{Any, CorsLayer};
use windrunner_server::actor::{self, Config};
use windrunner_server::{Auth, History, router, start_projection};
use windrunner_types::AccountId;

fn api_keys(spec: &str) -> Result<HashMap<String, AccountId>, String> {
    spec.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let (key, account) = entry
                .trim()
                .rsplit_once(':')
                .ok_or("WINDRUNNER_API_KEYS entries must look like key:account_id")?;
            let account = account
                .parse()
                .map_err(|_| format!("invalid account id in WINDRUNNER_API_KEYS: {account}"))?;
            Ok((key.to_owned(), AccountId(account)))
        })
        .collect()
}

fn cors(origins: &str) -> Result<CorsLayer, Box<dyn Error>> {
    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE]);
    if origins.trim() == "*" {
        return Ok(layer.allow_origin(Any));
    }
    let origins = origins
        .split(',')
        .map(|origin| origin.trim().parse::<HeaderValue>())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(layer.allow_origin(origins))
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt().init();
    let addr = env::var("WINDRUNNER_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let data_dir = env::var("WINDRUNNER_DATA_DIR").unwrap_or_else(|_| "data".into());
    let admin_key = env::var("WINDRUNNER_ADMIN_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or("WINDRUNNER_ADMIN_KEY must be set")?;
    let api_keys = api_keys(&env::var("WINDRUNNER_API_KEYS").unwrap_or_default())?;

    let history = match env::var("WINDRUNNER_DATABASE_URL") {
        Ok(url) => Some(History::connect(&url).await?),
        Err(_) => None,
    };
    let history_enabled = history.is_some();
    let projection = match &history {
        Some(history) => Some(start_projection(history).await?),
        None => None,
    };
    let exchange = actor::spawn(&data_dir, Config::default(), projection)?;
    let app = router(
        exchange,
        Auth {
            admin_key,
            api_keys,
        },
        history,
    );
    let app = match env::var("WINDRUNNER_CORS_ORIGINS") {
        Ok(origins) => app.layer(cors(&origins)?),
        Err(_) => app,
    };
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(addr = %listener.local_addr()?, data_dir, history = history_enabled, "listening");

    let (stop, stopped) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    });
    let mut graceful = stopped.clone();
    let mut deadline = stopped;
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = graceful.wait_for(|&stop| stop).await;
    });
    // Graceful shutdown waits for every connection, and a WebSocket client
    // may never hang up on its own.
    tokio::select! {
        result = serve => result?,
        _ = async {
            let _ = deadline.wait_for(|&stop| stop).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!("connections still open after the grace period, exiting"),
    }
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                _ = ctrl_c => {}
                _ = terminate.recv() => {}
            },
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
