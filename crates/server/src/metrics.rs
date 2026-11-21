use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Default)]
pub struct Metrics {
    pub inputs: AtomicU64,
    pub trades: AtomicU64,
    pub batches: AtomicU64,
    pub commit_micros: AtomicU64,
    pub queue_rejections: AtomicU64,
    pub sequence: AtomicU64,
    pub history_sequence: AtomicU64,
    pub ws_connections: AtomicU64,
}

impl Metrics {
    pub(crate) fn record_batch(&self, inputs: u64, commit: Duration) {
        self.inputs.fetch_add(inputs, Ordering::Relaxed);
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.commit_micros
            .fetch_add(commit.as_micros() as u64, Ordering::Relaxed);
    }

    /// Prometheus text exposition format.
    pub fn render(&self) -> String {
        let get = |value: &AtomicU64| value.load(Ordering::Relaxed);
        let commit_seconds = get(&self.commit_micros) as f64 / 1e6;
        let rows: [(&str, &str, &str, String); 8] = [
            (
                "inputs_total",
                "counter",
                "Inputs applied to the exchange.",
                get(&self.inputs).to_string(),
            ),
            (
                "trades_total",
                "counter",
                "Trades executed.",
                get(&self.trades).to_string(),
            ),
            (
                "batches_total",
                "counter",
                "Journal commits (one fsync each).",
                get(&self.batches).to_string(),
            ),
            (
                "commit_seconds_total",
                "counter",
                "Time spent in journal commits.",
                commit_seconds.to_string(),
            ),
            (
                "queue_rejections_total",
                "counter",
                "Requests refused because the queue was full.",
                get(&self.queue_rejections).to_string(),
            ),
            (
                "sequence",
                "gauge",
                "Sequence number of the last applied input.",
                get(&self.sequence).to_string(),
            ),
            (
                "history_sequence",
                "gauge",
                "Last input stored in the history database.",
                get(&self.history_sequence).to_string(),
            ),
            (
                "ws_connections",
                "gauge",
                "Open WebSocket connections.",
                get(&self.ws_connections).to_string(),
            ),
        ];
        let mut out = String::new();
        for (name, kind, help, value) in rows {
            let _ = writeln!(out, "# HELP windrunner_{name} {help}");
            let _ = writeln!(out, "# TYPE windrunner_{name} {kind}");
            let _ = writeln!(out, "windrunner_{name} {value}");
        }
        out
    }
}
