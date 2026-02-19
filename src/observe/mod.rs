pub mod alert;
pub mod correlation;
pub mod error;
pub mod ingest;
pub mod parquet;
pub mod proto;
pub mod query;
pub mod store;

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
