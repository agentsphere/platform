//! Integration tests for the OTLP ingest pipeline (ingest → flush → query).

mod helpers;

use axum::Router;
use axum::http::StatusCode;
use prost::Message;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::test_state;

// ---------------------------------------------------------------------------
// Custom test router that includes ingest endpoints + channels
// ---------------------------------------------------------------------------

fn ingest_test_router(
    state: platform::store::AppState,
    channels: platform::observe::ingest::IngestChannels,
) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(platform::api::router())
        .merge(platform::observe::router(channels))
        .merge(platform::registry::router())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build resource attributes, always including `platform.project_id`.
fn project_resource_attrs(
    service_name: &str,
    project_id: Uuid,
) -> Vec<platform::observe::proto::KeyValue> {
    vec![
        platform::observe::proto::KeyValue {
            key: "service.name".into(),
            value: Some(platform::observe::proto::AnyValue {
                value: Some(platform::observe::proto::any_value::Value::StringValue(
                    service_name.into(),
                )),
            }),
        },
        platform::observe::proto::KeyValue {
            key: "platform.project_id".into(),
            value: Some(platform::observe::proto::AnyValue {
                value: Some(platform::observe::proto::any_value::Value::StringValue(
                    project_id.to_string(),
                )),
            }),
        },
    ]
}

/// Build a minimal OTLP ExportTraceServiceRequest with one span.
fn build_trace_request(trace_id: &[u8; 16], span_id: &[u8; 8], project_id: Uuid) -> Vec<u8> {
    let request = platform::observe::proto::ExportTraceServiceRequest {
        resource_spans: vec![platform::observe::proto::ResourceSpans {
            resource: Some(platform::observe::proto::Resource {
                attributes: project_resource_attrs("ingest-test-svc", project_id),
                ..Default::default()
            }),
            scope_spans: vec![platform::observe::proto::ScopeSpans {
                spans: vec![platform::observe::proto::Span {
                    trace_id: trace_id.to_vec(),
                    span_id: span_id.to_vec(),
                    name: "test-span".into(),
                    kind: 1, // SERVER
                    start_time_unix_nano: 1_700_000_000_000_000_000,
                    end_time_unix_nano: 1_700_000_000_050_000_000,
                    status: Some(platform::observe::proto::SpanStatus {
                        code: 1,
                        message: String::new(),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

/// Build a trace request WITHOUT `platform.project_id` (for testing rejection).
fn build_trace_request_no_project(trace_id: &[u8; 16], span_id: &[u8; 8]) -> Vec<u8> {
    let request = platform::observe::proto::ExportTraceServiceRequest {
        resource_spans: vec![platform::observe::proto::ResourceSpans {
            resource: Some(platform::observe::proto::Resource {
                attributes: vec![platform::observe::proto::KeyValue {
                    key: "service.name".into(),
                    value: Some(platform::observe::proto::AnyValue {
                        value: Some(platform::observe::proto::any_value::Value::StringValue(
                            "no-project-svc".into(),
                        )),
                    }),
                }],
                ..Default::default()
            }),
            scope_spans: vec![platform::observe::proto::ScopeSpans {
                spans: vec![platform::observe::proto::Span {
                    trace_id: trace_id.to_vec(),
                    span_id: span_id.to_vec(),
                    name: "test-span".into(),
                    kind: 1,
                    start_time_unix_nano: 1_700_000_000_000_000_000,
                    end_time_unix_nano: 1_700_000_000_050_000_000,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

/// Build a minimal OTLP ExportLogsServiceRequest with one log record.
fn build_logs_request(project_id: Uuid) -> Vec<u8> {
    let request = platform::observe::proto::ExportLogsServiceRequest {
        resource_logs: vec![platform::observe::proto::ResourceLogs {
            resource: Some(platform::observe::proto::Resource {
                attributes: project_resource_attrs("ingest-log-svc", project_id),
                ..Default::default()
            }),
            scope_logs: vec![platform::observe::proto::ScopeLogs {
                log_records: vec![platform::observe::proto::LogRecord {
                    time_unix_nano: 1_700_000_000_000_000_000,
                    severity_number: 9, // INFO
                    severity_text: "INFO".into(),
                    body: Some(platform::observe::proto::AnyValue {
                        value: Some(platform::observe::proto::any_value::Value::StringValue(
                            "ingest test log message".into(),
                        )),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

/// Build a minimal OTLP ExportMetricsServiceRequest with one gauge.
fn build_metrics_request(metric_name: &str, project_id: Uuid) -> Vec<u8> {
    let request = platform::observe::proto::ExportMetricsServiceRequest {
        resource_metrics: vec![platform::observe::proto::ResourceMetrics {
            resource: Some(platform::observe::proto::Resource {
                attributes: project_resource_attrs("ingest-metric-svc", project_id),
                ..Default::default()
            }),
            scope_metrics: vec![platform::observe::proto::ScopeMetrics {
                metrics: vec![platform::observe::proto::Metric {
                    name: metric_name.into(),
                    unit: "bytes".into(),
                    data: Some(platform::observe::proto::metric_data::Data::Gauge(
                        platform::observe::proto::Gauge {
                            data_points: vec![platform::observe::proto::NumberDataPoint {
                                value: Some(
                                    platform::observe::proto::number_data_point::Value::AsDouble(
                                        42.5,
                                    ),
                                ),
                                time_unix_nano: 1_700_000_000_000_000_000,
                                ..Default::default()
                            }],
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

/// Send protobuf bytes to an ingest endpoint.
async fn post_protobuf(
    app: &Router,
    token: &str,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-protobuf")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap();

    let resp = ServiceExt::<axum::http::Request<Body>>::oneshot(app.clone(), req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

// ---------------------------------------------------------------------------
// Tests — existing ingest + flush + query (updated for project_id)
// ---------------------------------------------------------------------------

/// Ingest a trace via OTLP protobuf, flush, query via API.
#[sqlx::test(migrations = "./migrations")]
async fn ingest_traces_protobuf(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state.clone(), channels);

    // Create a project so the admin can ingest with a valid project_id
    let project_id = helpers::create_project(&app, &admin_token, "trace-proj", "private").await;

    let trace_id: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    let span_id: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    let body = build_trace_request(&trace_id, &span_id, project_id);

    let (status, _) = post_protobuf(&app, &admin_token, "/v1/traces", body).await;
    assert_eq!(status, StatusCode::OK);

    // Manually flush by spawning a short-lived flush
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let flush_pool = pool.clone();
    let handle = tokio::spawn(platform::observe::ingest::flush_spans(
        flush_pool,
        spans_rx,
        shutdown_rx,
    ));
    // Give the flush task a tick
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let _ = shutdown_tx.send(());
    let _ = handle.await;

    // Verify via query
    let expected_trace_id = "0102030405060708090a0b0c0d0e0f10";
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces/{expected_trace_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "trace not found: {body}");
    assert_eq!(body["trace_id"], expected_trace_id);
}

/// Ingest logs via OTLP protobuf, flush, query.
#[sqlx::test(migrations = "./migrations")]
async fn ingest_logs_protobuf(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state.clone(), channels);

    let project_id = helpers::create_project(&app, &admin_token, "logs-proj", "private").await;

    let body = build_logs_request(project_id);
    let (status, _) = post_protobuf(&app, &admin_token, "/v1/logs", body).await;
    assert_eq!(status, StatusCode::OK);

    // Flush
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(platform::observe::ingest::flush_logs(
        pool.clone(),
        state.valkey.clone(),
        logs_rx,
        shutdown_rx,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let _ = shutdown_tx.send(());
    let _ = handle.await;

    // Query
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/logs?service=ingest-log-svc",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "logs query failed: {body}");
    assert!(body["total"].as_i64().unwrap() >= 1);
}

/// Ingest metrics via OTLP protobuf, flush, query.
#[sqlx::test(migrations = "./migrations")]
async fn ingest_metrics_protobuf(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state.clone(), channels);

    let project_id = helpers::create_project(&app, &admin_token, "metrics-proj", "private").await;

    let metric_name = format!("ingest_test_{}", Uuid::new_v4().simple());
    let body = build_metrics_request(&metric_name, project_id);
    let (status, _) = post_protobuf(&app, &admin_token, "/v1/metrics", body).await;
    assert_eq!(status, StatusCode::OK);

    // Flush
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(platform::observe::ingest::flush_metrics(
        pool.clone(),
        metrics_rx,
        shutdown_rx,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let _ = shutdown_tx.send(());
    let _ = handle.await;

    // Query
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={metric_name}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "metrics query failed: {body}");
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(!series.is_empty(), "metric series should exist");
}

/// Invalid protobuf returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn ingest_invalid_protobuf_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state, channels);

    let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0x00];
    let (status, _) = post_protobuf(&app, &admin_token, "/v1/traces", garbage.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_protobuf(&app, &admin_token, "/v1/logs", garbage.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_protobuf(&app, &admin_token, "/v1/metrics", garbage).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Flush drains channel on shutdown signal.
#[sqlx::test(migrations = "./migrations")]
async fn flush_shutdown_drains_remaining(pool: PgPool) {
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    // Send a span record
    let span = platform::observe::store::SpanRecord {
        trace_id: "drain-trace".into(),
        span_id: "drain-span".into(),
        parent_span_id: None,
        name: "drain-test".into(),
        service: "drain-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(1),
        started_at: chrono::Utc::now(),
        finished_at: None,
        project_id: None,
        session_id: None,
        user_id: None,
    };
    tx.send(span).await.unwrap();
    drop(tx); // Close sender

    // Start flush with immediate shutdown
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let flush_pool = pool.clone();
    let handle = tokio::spawn(platform::observe::ingest::flush_spans(
        flush_pool,
        rx,
        shutdown_rx,
    ));

    // Give first tick a chance, then signal shutdown
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let _ = shutdown_tx.send(());
    let _ = handle.await;

    // Verify the span was written
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE service = 'drain-svc'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(count.0 >= 1, "span should have been flushed on shutdown");
}

// ---------------------------------------------------------------------------
// Tests — Phase 5A: per-project OTLP auth enforcement
// ---------------------------------------------------------------------------

/// OTLP ingest rejects payloads missing `platform.project_id` with 400.
#[sqlx::test(migrations = "./migrations")]
async fn otlp_ingest_missing_project_id_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state, channels);

    let trace_id: [u8; 16] = [10; 16];
    let span_id: [u8; 8] = [20; 8];
    let body = build_trace_request_no_project(&trace_id, &span_id);

    let (status, resp_bytes) = post_protobuf(&app, &admin_token, "/v1/traces", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let resp_text = String::from_utf8_lossy(&resp_bytes);
    assert!(
        resp_text.contains("platform.project_id"),
        "error should mention missing project_id attribute: {resp_text}"
    );
}

/// OTLP ingest rejects invalid (non-UUID) `platform.project_id` with 400.
#[sqlx::test(migrations = "./migrations")]
async fn otlp_ingest_invalid_project_id_uuid_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state, channels);

    // Build a trace with an invalid UUID as platform.project_id
    let request = platform::observe::proto::ExportTraceServiceRequest {
        resource_spans: vec![platform::observe::proto::ResourceSpans {
            resource: Some(platform::observe::proto::Resource {
                attributes: vec![
                    platform::observe::proto::KeyValue {
                        key: "service.name".into(),
                        value: Some(platform::observe::proto::AnyValue {
                            value: Some(platform::observe::proto::any_value::Value::StringValue(
                                "bad-uuid-svc".into(),
                            )),
                        }),
                    },
                    platform::observe::proto::KeyValue {
                        key: "platform.project_id".into(),
                        value: Some(platform::observe::proto::AnyValue {
                            value: Some(platform::observe::proto::any_value::Value::StringValue(
                                "not-a-valid-uuid".into(),
                            )),
                        }),
                    },
                ],
                ..Default::default()
            }),
            scope_spans: vec![platform::observe::proto::ScopeSpans {
                spans: vec![platform::observe::proto::Span {
                    trace_id: vec![99; 16],
                    span_id: vec![99; 8],
                    name: "bad-uuid-span".into(),
                    kind: 1,
                    start_time_unix_nano: 1_700_000_000_000_000_000,
                    end_time_unix_nano: 1_700_000_000_050_000_000,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let body = request.encode_to_vec();

    let (status, resp_bytes) = post_protobuf(&app, &admin_token, "/v1/traces", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let resp_text = String::from_utf8_lossy(&resp_bytes);
    assert!(
        resp_text.contains("not a valid UUID"),
        "error should mention invalid UUID: {resp_text}"
    );
}

/// OTLP ingest rejects user without ObserveWrite permission with 404 (not 403).
#[sqlx::test(migrations = "./migrations")]
async fn otlp_ingest_rejects_unauthorized_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state.clone(), channels);

    // Create a project owned by admin
    let project_id = helpers::create_project(&app, &admin_token, "auth-test-proj", "private").await;

    // Create a regular user with no roles (no ObserveWrite permission)
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "noobs-user", "noobs@test.com").await;

    // Attempt to ingest traces — should be rejected with 404 (avoids leaking existence)
    let trace_id: [u8; 16] = [30; 16];
    let span_id: [u8; 8] = [40; 8];
    let body = build_trace_request(&trace_id, &span_id, project_id);

    let (status, _) = post_protobuf(&app, &user_token, "/v1/traces", body).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unauthorized OTLP should return 404 to avoid leaking project existence"
    );
}

/// OTLP ingest accepts authorized user with ObserveWrite permission.
#[sqlx::test(migrations = "./migrations")]
async fn otlp_ingest_accepts_authorized_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state.clone(), channels);

    // Admin has all permissions — this tests the happy path with project_id
    let project_id = helpers::create_project(&app, &admin_token, "auth-ok-proj", "private").await;

    let trace_id: [u8; 16] = [50; 16];
    let span_id: [u8; 8] = [60; 8];
    let body = build_trace_request(&trace_id, &span_id, project_id);

    let (status, _) = post_protobuf(&app, &admin_token, "/v1/traces", body).await;
    assert_eq!(status, StatusCode::OK);
}

/// OTLP ingest handles multiple spans with same project_id efficiently (one auth check).
#[sqlx::test(migrations = "./migrations")]
async fn otlp_ingest_deduplicates_project_auth(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = platform::observe::ingest::create_channels();
    let app = ingest_test_router(state.clone(), channels);

    let project_id = helpers::create_project(&app, &admin_token, "dedup-proj", "private").await;

    // Build a request with two resource_spans referencing the same project
    let attrs = project_resource_attrs("dedup-svc", project_id);
    let request = platform::observe::proto::ExportTraceServiceRequest {
        resource_spans: vec![
            platform::observe::proto::ResourceSpans {
                resource: Some(platform::observe::proto::Resource {
                    attributes: attrs.clone(),
                    ..Default::default()
                }),
                scope_spans: vec![platform::observe::proto::ScopeSpans {
                    spans: vec![platform::observe::proto::Span {
                        trace_id: vec![1; 16],
                        span_id: vec![1; 8],
                        name: "span-a".into(),
                        kind: 1,
                        start_time_unix_nano: 1_700_000_000_000_000_000,
                        end_time_unix_nano: 1_700_000_000_050_000_000,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            platform::observe::proto::ResourceSpans {
                resource: Some(platform::observe::proto::Resource {
                    attributes: attrs,
                    ..Default::default()
                }),
                scope_spans: vec![platform::observe::proto::ScopeSpans {
                    spans: vec![platform::observe::proto::Span {
                        trace_id: vec![2; 16],
                        span_id: vec![2; 8],
                        name: "span-b".into(),
                        kind: 1,
                        start_time_unix_nano: 1_700_000_000_000_000_000,
                        end_time_unix_nano: 1_700_000_000_050_000_000,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
    };
    let body = request.encode_to_vec();

    // Should succeed — both resource_spans have the same project_id
    let (status, _) = post_protobuf(&app, &admin_token, "/v1/traces", body).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Tests — Phase 5B: OTLP token auto-creation
// ---------------------------------------------------------------------------

/// `ensure_otlp_token` creates a project-scoped API token with observe:write scope.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_otlp_token_creates_scoped_token(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let app = helpers::test_router(state.clone());
    let project_id =
        helpers::create_project(&app, &admin_token, "otlp-token-proj", "private").await;

    // Call ensure_otlp_token directly
    let raw_token = platform::deployer::reconciler::ensure_otlp_token(&state, project_id)
        .await
        .expect("ensure_otlp_token should succeed");

    // Verify the token was created
    assert!(
        raw_token.starts_with("plat_api_"),
        "token should have platform prefix"
    );

    // Verify DB entry: project_id, scopes, expiry
    let row: (Vec<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT scopes, project_id FROM api_tokens WHERE name LIKE 'otlp-auto-%' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("token row should exist");

    assert!(
        row.0.contains(&"observe:write".to_string()),
        "token should have observe:write scope, got: {:?}",
        row.0
    );
    assert_eq!(row.1, Some(project_id), "token should be scoped to project");
}

/// `ensure_otlp_token` rotates (replaces) existing tokens.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_otlp_token_rotates_existing(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;

    let app = helpers::test_router(state.clone());
    let project_id =
        helpers::create_project(&app, &admin_token, "otlp-rotate-proj", "private").await;

    // Create first token
    let token1 = platform::deployer::reconciler::ensure_otlp_token(&state, project_id)
        .await
        .expect("first ensure_otlp_token");

    // Create second token (should replace the first)
    let token2 = platform::deployer::reconciler::ensure_otlp_token(&state, project_id)
        .await
        .expect("second ensure_otlp_token");

    // Tokens should be different (new raw token each time)
    assert_ne!(token1, token2, "rotated tokens should differ");

    // Only one token should remain for this project
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM api_tokens WHERE project_id = $1 AND scopes @> ARRAY['observe:write']",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count.0, 1, "old token should be deleted after rotation");
}

/// `ensure_otlp_token` returns an error for a nonexistent project.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_otlp_token_nonexistent_project_returns_error(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;

    let fake_project_id = uuid::Uuid::new_v4();
    let result = platform::deployer::reconciler::ensure_otlp_token(&state, fake_project_id).await;

    assert!(result.is_err(), "should fail for nonexistent project");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("project not found"),
        "error should mention 'project not found', got: {err_msg}"
    );
}
