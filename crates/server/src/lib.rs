pub mod actor;
mod app;
pub mod metrics;
mod ws;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

pub use app::{Auth, router};
pub use windrunner_history::History;

/// Starts the background task that copies applied inputs into `history`.
/// Pass the result to `actor::spawn`.
pub async fn start_projection(
    history: &History,
) -> Result<actor::Projection, windrunner_history::Error> {
    let projected = Arc::new(AtomicU64::new(history.cursor().await?));
    let (sink, records) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(history.clone().run(records, projected.clone()));
    Ok(actor::Projection { sink, projected })
}
