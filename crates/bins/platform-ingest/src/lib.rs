// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Library interface for the platform-ingest binary.
//!
//! Re-exports modules and the router builder so integration tests can
//! construct the full axum router without starting a TCP listener.

pub mod auth;
pub mod config;
pub mod health;
pub mod state;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::routing::{get, post};
use axum::{Extension, Router};
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use platform_observe::alert::AlertRouter;
use platform_observe::ingest;
use platform_observe::proto;
use platform_types::ApiError;

use auth::IngestAuthUser;
use state::IngestState;

/// Build the ingest HTTP router.
pub fn build_router(state: IngestState, channels: ingest::IngestChannels) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz_handler))
        .route("/v1/traces", post(traces_handler))
        .route("/v1/logs", post(logs_handler))
        .route("/v1/metrics", post(metrics_handler))
        .layer(Extension(channels))
        .with_state(state)
}

/// Run the full ingest server: alert router, flush loops, and HTTP serve.
///
/// Blocks until `cancel` is triggered. The caller must provide a bound
/// `TcpListener`, a connected `PgPool`, and a connected Valkey pool.
pub async fn run(
    listener: TcpListener,
    pool: sqlx::PgPool,
    valkey: fred::clients::Pool,
    cancel: CancellationToken,
    trust_proxy: bool,
    buffer_capacity: usize,
) -> anyhow::Result<()> {
    let tracker = TaskTracker::new();
    let alert_router_degraded = Arc::new(AtomicBool::new(false));

    // Build initial AlertRouter for the ingest tap
    let alert_router = match AlertRouter::from_db(&pool).await {
        Ok(r) => Arc::new(RwLock::new(r)),
        Err(e) => {
            tracing::error!(error = %e, "failed to load alert router, starting empty");
            alert_router_degraded.store(true, std::sync::atomic::Ordering::Relaxed);
            Arc::new(RwLock::new(AlertRouter::empty()))
        }
    };

    // Alert rule subscriber — rebuilds router on rule changes
    tracker.spawn(platform_observe::alert::alert_rule_subscriber(
        pool.clone(),
        valkey.clone(),
        alert_router.clone(),
        cancel.clone(),
        Some(alert_router_degraded.clone()),
    ));

    let (channels, spans_rx, logs_rx, metrics_rx) =
        ingest::create_channels_with_capacity(buffer_capacity);

    tracker.spawn(ingest::flush_spans(
        pool.clone(),
        valkey.clone(),
        alert_router.clone(),
        spans_rx,
        cancel.clone(),
    ));
    tracker.spawn(ingest::flush_logs(
        pool.clone(),
        valkey.clone(),
        alert_router.clone(),
        logs_rx,
        cancel.clone(),
    ));
    tracker.spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let state = IngestState {
        pool,
        valkey,
        trust_proxy,
        alert_router_degraded,
    };
    let app = build_router(state, channels);

    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.clone().cancelled_owned())
        .await?;

    cancel.cancel();
    tracker.close();
    tracker.wait().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, channels, headers, body))]
async fn traces_handler(
    axum::extract::State(state): axum::extract::State<IngestState>,
    IngestAuthUser(auth): IngestAuthUser,
    Extension(channels): Extension<ingest::IngestChannels>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let rate_id = auth
        .boundary_project_id
        .map_or_else(|| auth.user_id.to_string(), |pid| pid.to_string());
    platform_auth::check_rate(&state.valkey, "otlp", &rate_id, 10_000, 60).await?;

    let body = ingest::maybe_decompress(&headers, body)?;
    let request = proto::ExportTraceServiceRequest::decode(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    let checker = platform_auth::PgPermissionChecker {
        pool: &state.pool,
        valkey: &state.valkey,
    };
    let resource_attrs_refs: Vec<&[proto::KeyValue]> = request
        .resource_spans
        .iter()
        .map(|rs| rs.resource.as_ref().map_or(&[][..], |r| &r.attributes[..]))
        .collect();
    ingest::check_otlp_project_auth(&auth, &checker, &resource_attrs_refs).await?;

    for rs in &request.resource_spans {
        let resource_attrs = rs.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                let record = ingest::build_span_record(span, resource_attrs, &state.pool).await;
                ingest::try_send_span(&channels, record)?;
            }
        }
    }

    let response_bytes = proto::ExportTraceServiceResponse {}.encode_to_vec();
    Ok((
        axum::http::StatusCode::OK,
        [("content-type", "application/x-protobuf")],
        response_bytes,
    ))
}

#[tracing::instrument(skip(state, channels, headers, body))]
async fn logs_handler(
    axum::extract::State(state): axum::extract::State<IngestState>,
    IngestAuthUser(auth): IngestAuthUser,
    Extension(channels): Extension<ingest::IngestChannels>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let rate_id = auth
        .boundary_project_id
        .map_or_else(|| auth.user_id.to_string(), |pid| pid.to_string());
    platform_auth::check_rate(&state.valkey, "otlp", &rate_id, 10_000, 60).await?;

    let body = ingest::maybe_decompress(&headers, body)?;
    let request = proto::ExportLogsServiceRequest::decode(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    let checker = platform_auth::PgPermissionChecker {
        pool: &state.pool,
        valkey: &state.valkey,
    };
    let resource_attrs_refs: Vec<&[proto::KeyValue]> = request
        .resource_logs
        .iter()
        .map(|rl| rl.resource.as_ref().map_or(&[][..], |r| &r.attributes[..]))
        .collect();
    ingest::check_otlp_project_auth(&auth, &checker, &resource_attrs_refs).await?;

    for rl in &request.resource_logs {
        let resource_attrs = rl.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for sl in &rl.scope_logs {
            for log in &sl.log_records {
                let record = ingest::build_log_record(log, resource_attrs, &state.pool).await;
                ingest::try_send_log(&channels, record)?;
            }
        }
    }

    let response_bytes = proto::ExportLogsServiceResponse {}.encode_to_vec();
    Ok((
        axum::http::StatusCode::OK,
        [("content-type", "application/x-protobuf")],
        response_bytes,
    ))
}

#[tracing::instrument(skip(state, channels, headers, body))]
async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<IngestState>,
    IngestAuthUser(auth): IngestAuthUser,
    Extension(channels): Extension<ingest::IngestChannels>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let rate_id = auth
        .boundary_project_id
        .map_or_else(|| auth.user_id.to_string(), |pid| pid.to_string());
    platform_auth::check_rate(&state.valkey, "otlp", &rate_id, 10_000, 60).await?;

    let body = ingest::maybe_decompress(&headers, body)?;
    let request = proto::ExportMetricsServiceRequest::decode(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    let checker = platform_auth::PgPermissionChecker {
        pool: &state.pool,
        valkey: &state.valkey,
    };
    let resource_attrs_refs: Vec<&[proto::KeyValue]> = request
        .resource_metrics
        .iter()
        .map(|rm| rm.resource.as_ref().map_or(&[][..], |r| &r.attributes[..]))
        .collect();
    ingest::check_otlp_project_auth(&auth, &checker, &resource_attrs_refs).await?;

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

    let response_bytes = proto::ExportMetricsServiceResponse {}.encode_to_vec();
    Ok((
        axum::http::StatusCode::OK,
        [("content-type", "application/x-protobuf")],
        response_bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_router_does_not_panic() {
        let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(1);
        let state = state::IngestState {
            pool: sqlx::PgPool::connect_lazy("postgres://fake:fake@localhost/fake")
                .expect("lazy pool"),
            valkey: fred::clients::Pool::new(
                fred::types::config::Config::default(),
                None,
                None,
                None,
                1,
            )
            .expect("valkey pool"),
            trust_proxy: false,
            alert_router_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let _router: Router = build_router(state, channels);
    }

    #[test]
    fn config_defaults() {
        use clap::Parser;
        let cfg = config::IngestConfig::try_parse_from([
            "platform-ingest",
            "--database-url",
            "postgres://x@localhost/x",
            "--valkey-url",
            "redis://localhost:6379",
        ])
        .expect("parse config");

        assert_eq!(cfg.listen, "0.0.0.0:8081");
        assert!(!cfg.trust_proxy);
        assert_eq!(cfg.buffer_capacity, 10_000);
        assert_eq!(cfg.permission_cache_ttl, 300);
    }

    #[test]
    fn config_override_all() {
        use clap::Parser;
        let cfg = config::IngestConfig::try_parse_from([
            "platform-ingest",
            "--database-url",
            "postgres://x@localhost/x",
            "--valkey-url",
            "redis://localhost:6379",
            "--listen",
            "127.0.0.1:9999",
            "--trust-proxy",
            "--buffer-capacity",
            "500",
            "--permission-cache-ttl",
            "60",
        ])
        .expect("parse config");

        assert_eq!(cfg.listen, "127.0.0.1:9999");
        assert!(cfg.trust_proxy);
        assert_eq!(cfg.buffer_capacity, 500);
        assert_eq!(cfg.permission_cache_ttl, 60);
    }

    #[test]
    fn ingest_state_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<state::IngestState>();
    }
}
