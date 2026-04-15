// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Observe domain: OTLP proto types, record types, store writes,
//! correlation, ingest pipeline, alert state machine, Parquet rotation,
//! partition management, and query endpoints.

pub mod alert;
mod auth;
pub mod correlation;
pub mod error;
pub mod ingest;
pub mod parquet;
pub mod partitions;
pub mod proto;
pub mod query;
pub mod state;
pub mod store;
pub mod types;

use axum::Router;

use crate::state::ObserveState;

/// Build the full observe router (ingest + query + alerts).
pub fn router(channels: ingest::IngestChannels) -> Router<ObserveState> {
    Router::new()
        .route("/v1/traces", axum::routing::post(ingest_traces_handler))
        .route("/v1/logs", axum::routing::post(ingest_logs_handler))
        .route("/v1/metrics", axum::routing::post(ingest_metrics_handler))
        .layer(axum::Extension(channels))
        .merge(query::router())
}

/// Spawn all observe background tasks. Returns `IngestChannels` for the router.
pub fn spawn_background_tasks(
    state: &ObserveState,
    cancel: tokio_util::sync::CancellationToken,
    tracker: &tokio_util::task::TaskTracker,
) -> ingest::IngestChannels {
    let (channels, spans_rx, logs_rx, metrics_rx) =
        ingest::create_channels_with_capacity(state.config.buffer_capacity);

    tracker.spawn(ingest::flush_spans(
        state.pool.clone(),
        state.valkey.clone(),
        state.alert_router.clone(),
        spans_rx,
        cancel.clone(),
    ));
    tracker.spawn(ingest::flush_logs(
        state.pool.clone(),
        state.valkey.clone(),
        state.alert_router.clone(),
        logs_rx,
        cancel.clone(),
    ));
    tracker.spawn(ingest::flush_metrics(
        state.pool.clone(),
        state.valkey.clone(),
        state.alert_router.clone(),
        metrics_rx,
        cancel.clone(),
    ));
    tracker.spawn(parquet::rotation_loop(state.clone(), cancel.clone()));

    tracker.spawn(retention_loop(
        state.pool.clone(),
        state.config.retention_days,
        cancel.clone(),
    ));

    // Alert rule subscriber — rebuilds AlertRouter on rule changes (every replica)
    tracker.spawn(alert::alert_rule_subscriber(
        state.pool.clone(),
        state.valkey.clone(),
        state.alert_router.clone(),
        cancel.clone(),
        None,
    ));

    // Stream alert evaluator — replaces the poll-based evaluate_alerts_loop.
    // Uses a consumer group so only one replica is active leader.
    tracker.spawn(alert::stream_alert_evaluator(
        state.pool.clone(),
        state.valkey.clone(),
        cancel.clone(),
        state.config.alert_max_window_secs,
    ));

    tracker.spawn(partitions::run(state.pool.clone(), cancel));

    channels
}

/// Observability data retention — purge old data hourly.
async fn retention_loop(
    pool: sqlx::PgPool,
    retention_days: u32,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let cutoff = chrono::Utc::now()
                    - chrono::Duration::days(i64::from(retention_days));
                for (table, col) in &[
                    ("spans", "started_at"),
                    ("log_entries", "timestamp"),
                    ("metric_samples", "timestamp"),
                ] {
                    let batch_size: i64 = 50_000;
                    let mut total_deleted: u64 = 0;
                    loop {
                        let sql = format!(
                            "DELETE FROM {table} WHERE ctid IN (\
                                SELECT ctid FROM {table} WHERE {col} < $1 LIMIT $2\
                            )"
                        );
                        match sqlx::query(&sql)
                            .bind(cutoff)
                            .bind(batch_size)
                            .execute(&pool)
                            .await
                        {
                            Ok(result) => {
                                let deleted = result.rows_affected();
                                total_deleted += deleted;
                                #[allow(clippy::cast_sign_loss)]
                                if deleted < batch_size as u64 {
                                    break;
                                }
                                tokio::time::sleep(
                                    std::time::Duration::from_millis(100),
                                )
                                .await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    table,
                                    error = %e,
                                    "retention cleanup batch failed"
                                );
                                break;
                            }
                        }
                    }
                    if total_deleted > 0 {
                        tracing::info!(
                            table,
                            rows = total_deleted,
                            retention_days,
                            "purged old observability data"
                        );
                    }
                }
            }
            () = cancel.cancelled() => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Thin ingest handler wrappers that bridge ObserveState → ingest functions
// ---------------------------------------------------------------------------

/// OTLP traces ingest handler for the crate router.
///
/// Uses `ObserveState` and delegates to the shared ingest logic.
async fn ingest_traces_handler(
    axum::extract::State(state): axum::extract::State<ObserveState>,
    auth: platform_types::AuthUser,
    axum::Extension(channels): axum::Extension<ingest::IngestChannels>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::http::StatusCode, platform_types::ApiError> {
    let body = ingest::maybe_decompress(&headers, body)?;
    let request: proto::ExportTraceServiceRequest = prost::Message::decode(body)
        .map_err(|e| platform_types::ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    let checker = platform_auth::PgPermissionChecker {
        pool: &state.pool,
        valkey: &state.valkey,
    };

    let resource_attrs_list: Vec<&[proto::KeyValue]> = request
        .resource_spans
        .iter()
        .filter_map(|rs| rs.resource.as_ref().map(|r| r.attributes.as_slice()))
        .collect();

    ingest::check_otlp_project_auth(&auth, &checker, &resource_attrs_list).await?;

    for rs in &request.resource_spans {
        let resource_attrs = rs.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                let record = ingest::build_span_record(span, resource_attrs, &state.pool).await;
                ingest::try_send_span(&channels, record)?;
            }
        }
    }

    Ok(axum::http::StatusCode::OK)
}

/// OTLP logs ingest handler for the crate router.
async fn ingest_logs_handler(
    axum::extract::State(state): axum::extract::State<ObserveState>,
    auth: platform_types::AuthUser,
    axum::Extension(channels): axum::Extension<ingest::IngestChannels>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::http::StatusCode, platform_types::ApiError> {
    let body = ingest::maybe_decompress(&headers, body)?;
    let request: proto::ExportLogsServiceRequest = prost::Message::decode(body)
        .map_err(|e| platform_types::ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    let checker = platform_auth::PgPermissionChecker {
        pool: &state.pool,
        valkey: &state.valkey,
    };

    let resource_attrs_list: Vec<&[proto::KeyValue]> = request
        .resource_logs
        .iter()
        .filter_map(|rl| rl.resource.as_ref().map(|r| r.attributes.as_slice()))
        .collect();

    ingest::check_otlp_project_auth(&auth, &checker, &resource_attrs_list).await?;

    for rl in &request.resource_logs {
        let resource_attrs = rl.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for sl in &rl.scope_logs {
            for log in &sl.log_records {
                let record = ingest::build_log_record(log, resource_attrs, &state.pool).await;
                ingest::try_send_log(&channels, record)?;
            }
        }
    }

    Ok(axum::http::StatusCode::OK)
}

/// OTLP metrics ingest handler for the crate router.
async fn ingest_metrics_handler(
    axum::extract::State(state): axum::extract::State<ObserveState>,
    auth: platform_types::AuthUser,
    axum::Extension(channels): axum::Extension<ingest::IngestChannels>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::http::StatusCode, platform_types::ApiError> {
    let body = ingest::maybe_decompress(&headers, body)?;
    let request: proto::ExportMetricsServiceRequest = prost::Message::decode(body)
        .map_err(|e| platform_types::ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    let checker = platform_auth::PgPermissionChecker {
        pool: &state.pool,
        valkey: &state.valkey,
    };

    let resource_attrs_list: Vec<&[proto::KeyValue]> = request
        .resource_metrics
        .iter()
        .filter_map(|rm| rm.resource.as_ref().map(|r| r.attributes.as_slice()))
        .collect();

    ingest::check_otlp_project_auth(&auth, &checker, &resource_attrs_list).await?;

    for rm in &request.resource_metrics {
        let resource_attrs = rm.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for sm in &rm.scope_metrics {
            for metric in &sm.metrics {
                let records =
                    ingest::build_metric_records(metric, resource_attrs, &state.pool).await;
                for record in records {
                    ingest::try_send_metric(&channels, record)?;
                }
            }
        }
    }

    Ok(axum::http::StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_builds_without_panic() {
        let (channels, _spans_rx, _logs_rx, _metrics_rx) =
            ingest::create_channels_with_capacity(100);
        let _router: Router<ObserveState> = router(channels);
    }
}
