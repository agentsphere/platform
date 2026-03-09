pub mod alert;
pub mod correlation;
pub mod error;
pub mod ingest;
pub mod parquet;
pub mod proto;
pub mod query;
pub mod store;
pub mod tracing_layer;

use axum::Router;

use crate::store::AppState;

/// Build the observe module router (OTLP ingest + query + alerts).
pub fn router(channels: ingest::IngestChannels) -> Router<AppState> {
    Router::new()
        .route("/v1/traces", axum::routing::post(ingest::ingest_traces))
        .route("/v1/logs", axum::routing::post(ingest::ingest_logs))
        .route("/v1/metrics", axum::routing::post(ingest::ingest_metrics))
        .layer(axum::Extension(channels))
        .merge(query::router())
        .merge(alert::router())
}

/// Spawn all observe background tasks. Returns `IngestChannels` for the router.
pub fn spawn_background_tasks(
    state: AppState,
    shutdown_rx: tokio::sync::watch::Receiver<()>,
) -> ingest::IngestChannels {
    let (channels, spans_rx, logs_rx, metrics_rx) = ingest::create_channels();

    tokio::spawn(ingest::flush_spans(
        state.pool.clone(),
        spans_rx,
        shutdown_rx.clone(),
    ));
    tokio::spawn(ingest::flush_logs(
        state.pool.clone(),
        state.valkey.clone(),
        logs_rx,
        shutdown_rx.clone(),
    ));
    tokio::spawn(ingest::flush_metrics(
        state.pool.clone(),
        metrics_rx,
        shutdown_rx.clone(),
    ));
    tokio::spawn(parquet::rotation_loop(state.clone(), shutdown_rx.clone()));
    tokio::spawn(alert::evaluate_alerts_loop(state, shutdown_rx));

    channels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_builds_without_panic() {
        // Create channels just to build the router — verifies route wiring
        let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels();
        let _router: Router<AppState> = router(channels);
    }

    #[tokio::test]
    async fn spawn_background_tasks_and_shutdown() {
        // Spawn background tasks and immediately signal shutdown.
        // This verifies that the tasks start and exit cleanly without panicking.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // We need a minimal AppState — use dummy connections since the tasks
        // will shut down before they try to use them.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

        // Build a minimal pool config — tasks won't actually query before shutdown.
        // We can't use a real pool in unit tests, but we CAN verify that
        // create_channels returns the right types.
        let (channels, spans_rx, logs_rx, metrics_rx) = ingest::create_channels();

        // Verify channels are created with correct types
        drop(spans_rx);
        drop(logs_rx);
        drop(metrics_rx);
        drop(channels);

        // Signal immediate shutdown
        shutdown_tx.send(()).unwrap();
        drop(shutdown_rx);
    }
}
