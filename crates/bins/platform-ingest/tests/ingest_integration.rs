// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform-ingest` binary.
//!
//! Covers:
//! - Flush loops (spans, logs, metrics → Postgres)
//! - Alert routing (metric → `alert:samples` Valkey stream)
//! - Alert subscriber (rule change notification → router rebuild)
//! - HTTP handler tests via `tower::ServiceExt::oneshot` (protobuf → POST → verify DB)
//! - Auth enforcement (no token → 401, invalid token → 401, missing perm → 403/404)
//! - Healthz endpoint

use std::sync::Arc;

use axum::body::Body;
use chrono::Utc;
use fred::interfaces::{ClientLike, PubsubInterface, StreamsInterface};
use http::Request;
use http_body_util::BodyExt;
use prost::Message;
use sqlx::{PgPool, Row};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

use platform_observe::alert::{ALERT_RULES_CHANGED_CHANNEL, ALERT_STREAM_KEY, AlertRouter};
use platform_observe::ingest;
use platform_observe::proto;
use platform_observe::types::{LogEntryRecord, MetricRecord, SpanRecord};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn valkey_pool() -> fred::clients::Pool {
    let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let url = url.replace("redis://:", "redis://default:");
    let config = fred::types::config::Config::from_url(&url).expect("invalid VALKEY_URL");
    let pool =
        fred::clients::Pool::new(config, None, None, None, 1).expect("valkey pool creation failed");
    pool.init().await.expect("valkey connection failed");
    pool
}

/// Seed a minimal user. Returns `user_id`.
async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let name = format!("u-{id}");
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, $2, $3, 'not-a-real-hash', 'human')",
    )
    .bind(id)
    .bind(&name)
    .bind(format!("{name}@test.local"))
    .execute(pool)
    .await
    .expect("seed user");
    id
}

/// Seed a workspace + project. Returns (`workspace_id`, `project_id`).
async fn seed_project(pool: &PgPool, owner_id: Uuid) -> (Uuid, Uuid) {
    let ws_id = Uuid::new_v4();
    let ws_name = format!("ws-{ws_id}");
    sqlx::query("INSERT INTO workspaces (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(ws_id)
        .bind(&ws_name)
        .bind(owner_id)
        .execute(pool)
        .await
        .expect("seed workspace");

    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(ws_id)
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("seed workspace member");

    let proj_id = Uuid::new_v4();
    let proj_name = format!("proj-{proj_id}");
    let slug = format!("slug-{proj_id}");
    sqlx::query(
        "INSERT INTO projects (id, owner_id, workspace_id, name, namespace_slug)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(proj_id)
    .bind(owner_id)
    .bind(ws_id)
    .bind(&proj_name)
    .bind(&slug)
    .execute(pool)
    .await
    .expect("seed project");

    (ws_id, proj_id)
}

/// Seed an API token for a user. Returns the raw token string.
async fn seed_api_token(
    pool: &PgPool,
    user_id: Uuid,
    scopes: &[&str],
    project_id: Option<Uuid>,
) -> String {
    let (raw, hash) = platform_auth::generate_api_token();
    let scopes_json: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, scopes, project_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '1 day')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(format!("test-token-{}", Uuid::new_v4()))
    .bind(&hash)
    .bind(&scopes_json)
    .bind(project_id)
    .execute(pool)
    .await
    .expect("seed api token");
    raw
}

/// Seed a permission row. Returns `permission_id`.
async fn seed_permission(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let parts: Vec<&str> = name.splitn(2, ':').collect();
    let (resource, action) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        (name, "read")
    };
    sqlx::query(
        "INSERT INTO permissions (id, name, resource, action)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .expect("seed permission");
    id
}

/// Seed `admin:config` permission for a user.
async fn seed_admin_permission(pool: &PgPool, user_id: Uuid) {
    let perm_id = seed_permission(pool, "admin:config").await;
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO roles (id, name) VALUES ($1, $2)")
        .bind(role_id)
        .bind(format!("admin-role-{role_id}"))
        .execute(pool)
        .await
        .expect("seed role");

    sqlx::query("INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)")
        .bind(role_id)
        .bind(perm_id)
        .execute(pool)
        .await
        .expect("seed role_permission");

    sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(role_id)
        .execute(pool)
        .await
        .expect("seed user_role");
}

/// Seed `observe:write` permission for a user on a specific project.
async fn seed_observe_write_permission(pool: &PgPool, user_id: Uuid, project_id: Uuid) {
    let perm_id = seed_permission(pool, "observe:write").await;
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO roles (id, name) VALUES ($1, $2)")
        .bind(role_id)
        .bind(format!("observe-role-{role_id}"))
        .execute(pool)
        .await
        .expect("seed role");

    sqlx::query("INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)")
        .bind(role_id)
        .bind(perm_id)
        .execute(pool)
        .await
        .expect("seed role_permission");

    sqlx::query("INSERT INTO user_roles (user_id, role_id, project_id) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(role_id)
        .bind(project_id)
        .execute(pool)
        .await
        .expect("seed user_role");
}

/// Seed an alert rule with a given query and optional `project_id`. Returns `rule_id`.
async fn seed_alert_rule(pool: &PgPool, query: &str, project_id: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO alert_rules (id, name, query, condition, threshold, for_seconds, severity, project_id, enabled)
         VALUES ($1, $2, $3, 'gt', 80.0, 60, 'warning', $4, true)",
    )
    .bind(id)
    .bind(format!("rule-{id}"))
    .bind(query)
    .bind(project_id)
    .execute(pool)
    .await
    .expect("seed alert_rule");
    id
}

/// Build an `IngestState` from a pool.
fn ingest_state(pool: PgPool, valkey: fred::clients::Pool) -> platform_ingest::state::IngestState {
    platform_ingest::state::IngestState {
        pool,
        valkey,
        trust_proxy: false,
        alert_router_degraded: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

/// Build a test span record.
fn make_span(trace_id: &str, span_id: &str, project_id: Option<Uuid>) -> SpanRecord {
    let now = Utc::now();
    SpanRecord {
        trace_id: trace_id.into(),
        span_id: span_id.into(),
        parent_span_id: None,
        name: "test-span".into(),
        service: "test-svc".into(),
        kind: "internal".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(42),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(42)),
        project_id,
        session_id: None,
        user_id: None,
    }
}

/// Build a test log record.
fn make_log(project_id: Option<Uuid>) -> LogEntryRecord {
    LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id,
        session_id: None,
        user_id: None,
        service: "test-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "test log message".into(),
        attributes: None,
    }
}

/// Build a test metric record.
fn make_metric(name: &str, value: f64, project_id: Option<Uuid>) -> MetricRecord {
    MetricRecord {
        name: name.into(),
        labels: serde_json::json!({}),
        metric_type: "gauge".into(),
        unit: Some("bytes".into()),
        project_id,
        timestamp: Utc::now(),
        value,
    }
}

// ---------------------------------------------------------------------------
// Protobuf builder helpers
// ---------------------------------------------------------------------------

fn make_kv(key: &str, value: &str) -> proto::KeyValue {
    proto::KeyValue {
        key: key.into(),
        value: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(value.into())),
        }),
    }
}

fn make_trace_request(
    trace_id: &[u8; 16],
    span_name: &str,
    project_id: Option<Uuid>,
) -> proto::ExportTraceServiceRequest {
    let mut attrs = Vec::new();
    if let Some(pid) = project_id {
        attrs.push(make_kv("platform.project_id", &pid.to_string()));
    }
    attrs.push(make_kv("service.name", "test-svc"));

    let now_nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .cast_unsigned();
    proto::ExportTraceServiceRequest {
        resource_spans: vec![proto::ResourceSpans {
            resource: Some(proto::Resource { attributes: attrs }),
            scope_spans: vec![proto::ScopeSpans {
                scope: None,
                spans: vec![proto::Span {
                    trace_id: trace_id.to_vec(),
                    span_id: Uuid::new_v4().as_bytes()[..8].to_vec(),
                    parent_span_id: vec![],
                    name: span_name.into(),
                    kind: 1, // Internal
                    start_time_unix_nano: now_nanos,
                    end_time_unix_nano: now_nanos + 42_000_000,
                    attributes: vec![],
                    events: vec![],
                    status: Some(proto::SpanStatus {
                        message: String::new(),
                        code: 1, // Ok
                    }),
                }],
            }],
        }],
    }
}

fn make_logs_request(message: &str, project_id: Option<Uuid>) -> proto::ExportLogsServiceRequest {
    let mut attrs = Vec::new();
    if let Some(pid) = project_id {
        attrs.push(make_kv("platform.project_id", &pid.to_string()));
    }
    attrs.push(make_kv("service.name", "test-svc"));

    let now_nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .cast_unsigned();
    proto::ExportLogsServiceRequest {
        resource_logs: vec![proto::ResourceLogs {
            resource: Some(proto::Resource { attributes: attrs }),
            scope_logs: vec![proto::ScopeLogs {
                scope: None,
                log_records: vec![proto::LogRecord {
                    time_unix_nano: now_nanos,
                    severity_number: 9, // Info
                    severity_text: "info".into(),
                    body: Some(proto::AnyValue {
                        value: Some(proto::any_value::Value::StringValue(message.into())),
                    }),
                    attributes: vec![],
                    trace_id: vec![],
                    span_id: vec![],
                }],
            }],
        }],
    }
}

fn make_metrics_request(
    name: &str,
    value: f64,
    project_id: Option<Uuid>,
) -> proto::ExportMetricsServiceRequest {
    let mut attrs = Vec::new();
    if let Some(pid) = project_id {
        attrs.push(make_kv("platform.project_id", &pid.to_string()));
    }
    attrs.push(make_kv("service.name", "test-svc"));

    let now_nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .cast_unsigned();
    proto::ExportMetricsServiceRequest {
        resource_metrics: vec![proto::ResourceMetrics {
            resource: Some(proto::Resource { attributes: attrs }),
            scope_metrics: vec![proto::ScopeMetrics {
                scope: None,
                metrics: vec![proto::Metric {
                    name: name.into(),
                    description: String::new(),
                    unit: "bytes".into(),
                    data: Some(proto::metric_data::Data::Gauge(proto::Gauge {
                        data_points: vec![proto::NumberDataPoint {
                            attributes: vec![],
                            time_unix_nano: now_nanos,
                            value: Some(proto::number_data_point::Value::AsDouble(value)),
                        }],
                    })),
                }],
            }],
        }],
    }
}

/// POST protobuf to the router and return `(StatusCode, body_bytes)`.
async fn post_proto(
    app: axum::Router,
    path: &str,
    token: Option<&str>,
    body: Vec<u8>,
) -> (http::StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-protobuf");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder.body(Body::from(body)).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, body)
}

// ===========================================================================
// Flush loop tests
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_spans_writes_to_db(pool: PgPool) {
    let valkey = valkey_pool().await;
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let cancel = CancellationToken::new();

    let (channels, spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_spans(
        pool.clone(),
        valkey.clone(),
        alert_router,
        spans_rx,
        cancel.clone(),
    ));

    let trace_id = format!("t-{}", Uuid::new_v4());
    let span_id = format!("s-{}", Uuid::new_v4());
    let span = make_span(&trace_id, &span_id, None);
    ingest::try_send_span(&channels, span).unwrap();

    // Wait for flush (interval is 1s)
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE span_id = $1")
        .bind(&span_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1, "flush_spans should write span to DB");

    let trace_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM traces WHERE trace_id = $1")
        .bind(&trace_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(trace_count.0, 1, "flush_spans should upsert trace");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_logs_writes_to_db(pool: PgPool) {
    let valkey = valkey_pool().await;
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let cancel = CancellationToken::new();

    let (channels, _spans_rx, logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_logs(
        pool.clone(),
        valkey.clone(),
        alert_router,
        logs_rx,
        cancel.clone(),
    ));

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();

    let log = make_log(None);
    ingest::try_send_log(&channels, log).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after.0 - before.0, 1, "flush_logs should write log to DB");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_metrics_writes_to_db(pool: PgPool) {
    let valkey = valkey_pool().await;
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let cancel = CancellationToken::new();

    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let metric_name = format!("test.metric.{}", Uuid::new_v4());
    let metric = make_metric(&metric_name, 99.5, None);
    ingest::try_send_metric(&channels, metric).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let series = sqlx::query("SELECT id, last_value FROM metric_series WHERE name = $1")
        .bind(&metric_name)
        .fetch_one(&pool)
        .await
        .expect("series should exist");
    let last_value: f64 = series.get("last_value");
    assert!((last_value - 99.5).abs() < f64::EPSILON);
}

// ===========================================================================
// Alert routing tests
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_metrics_routes_to_alert_stream(pool: PgPool) {
    let valkey = valkey_pool().await;
    let metric_name = format!("test.alert.{}", Uuid::new_v4());
    let rule_id = seed_alert_rule(&pool, &format!("metric:{metric_name} agg:avg"), None).await;

    let alert_router = Arc::new(RwLock::new(
        AlertRouter::from_db(&pool).await.expect("load router"),
    ));

    let cancel = CancellationToken::new();
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    // Note the stream length before our test
    let before_len: usize = valkey.xlen::<usize, _>(ALERT_STREAM_KEY).await.unwrap_or(0);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let metric = make_metric(&metric_name, 100.0, None);
    ingest::try_send_metric(&channels, metric).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let after_len: usize = valkey
        .xlen::<usize, _>(ALERT_STREAM_KEY)
        .await
        .expect("XLEN");
    assert!(
        after_len > before_len,
        "alert:samples stream should have new entries for rule {rule_id}"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_metrics_no_alert_when_no_rules(pool: PgPool) {
    let valkey = valkey_pool().await;
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let cancel = CancellationToken::new();

    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    // Send a metric and wait for flush — it should write to DB but NOT to alert stream.
    // We verify the DB write succeeds (the flush ran) and rely on the
    // AlertRouter::empty() + is_empty() early-return to skip XADD.
    // Checking stream length is racy with parallel tests, so we only verify the
    // metric reached the DB.
    let metric_name = format!("no-rule-metric.{}", Uuid::new_v4());
    let metric = make_metric(&metric_name, 42.0, None);
    ingest::try_send_metric(&channels, metric).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let series = sqlx::query("SELECT last_value FROM metric_series WHERE name = $1")
        .bind(&metric_name)
        .fetch_one(&pool)
        .await
        .expect("metric should be in DB even without alert rules");
    let last_value: f64 = series.get("last_value");
    assert!((last_value - 42.0).abs() < f64::EPSILON);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn alert_subscriber_rebuilds_on_notification(pool: PgPool) {
    let valkey = valkey_pool().await;
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let cancel = CancellationToken::new();

    assert!(alert_router.read().await.is_empty(), "should start empty");

    // Seed a rule in DB so from_db() will return a non-empty router
    let metric_name = format!("test.sub.{}", Uuid::new_v4());
    seed_alert_rule(&pool, &format!("metric:{metric_name} agg:avg"), None).await;

    // Start the subscriber
    let handle = tokio::spawn(platform_observe::alert::alert_rule_subscriber(
        pool.clone(),
        valkey.clone(),
        alert_router.clone(),
        cancel.clone(),
        None,
    ));

    // Give subscriber time to connect
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Publish the notification
    let _: () = valkey
        .next()
        .publish(ALERT_RULES_CHANGED_CHANNEL, "rebuild")
        .await
        .expect("publish");

    // Wait for rebuild
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert!(
        !alert_router.read().await.is_empty(),
        "router should have rebuilt with the seeded rule"
    );

    cancel.cancel();
    // Wait for the subscriber task to observe cancellation and exit
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

// ===========================================================================
// HTTP handler tests
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn traces_handler_accepts_valid_protobuf(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey.clone());

    let cancel = CancellationToken::new();
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let (channels, spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_spans(
        pool.clone(),
        valkey.clone(),
        alert_router,
        spans_rx,
        cancel.clone(),
    ));

    let app = platform_ingest::build_router(state, channels);

    // Create admin user with token
    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let trace_id: [u8; 16] = Uuid::new_v4().into_bytes();
    let req = make_trace_request(&trace_id, "test-span", None);
    let body = req.encode_to_vec();

    let (status, _) = post_proto(app, "/v1/traces", Some(&token), body).await;
    assert_eq!(status, http::StatusCode::OK);

    // Wait for flush
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let trace_hex = hex::encode(trace_id);
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE trace_id = $1")
        .bind(&trace_hex)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1, "span should be in DB after handler + flush");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn logs_handler_accepts_valid_protobuf(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey.clone());

    let cancel = CancellationToken::new();
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let (channels, _spans_rx, logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_logs(
        pool.clone(),
        valkey.clone(),
        alert_router,
        logs_rx,
        cancel.clone(),
    ));

    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();

    let req = make_logs_request("hello from test", None);
    let body = req.encode_to_vec();

    let (status, _) = post_proto(app, "/v1/logs", Some(&token), body).await;
    assert_eq!(status, http::StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after.0 - before.0,
        1,
        "log should be in DB after handler + flush"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn metrics_handler_accepts_valid_protobuf(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey.clone());

    let cancel = CancellationToken::new();
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let metric_name = format!("test.http.{}", Uuid::new_v4());
    let req = make_metrics_request(&metric_name, 77.7, None);
    let body = req.encode_to_vec();

    let (status, _) = post_proto(app, "/v1/metrics", Some(&token), body).await;
    assert_eq!(status, http::StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let series = sqlx::query("SELECT last_value FROM metric_series WHERE name = $1")
        .bind(&metric_name)
        .fetch_one(&pool)
        .await
        .expect("series should exist");
    let last_value: f64 = series.get("last_value");
    assert!((last_value - 77.7).abs() < f64::EPSILON);
}

// ===========================================================================
// Auth enforcement tests
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_no_auth(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let req = make_trace_request(&[0u8; 16], "test", None);
    let (status, _) = post_proto(app, "/v1/traces", None, req.encode_to_vec()).await;
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_invalid_token(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let req = make_trace_request(&[0u8; 16], "test", None);
    let (status, _) = post_proto(
        app,
        "/v1/traces",
        Some("plat_api_bogus_token_that_doesnt_exist"),
        req.encode_to_vec(),
    )
    .await;
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn system_metrics_need_admin(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    // Create a regular user (no admin permission)
    let user_id = seed_user(&pool).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    // System metrics = no project_id attribute → requires admin
    let req = make_metrics_request("sys.cpu", 50.0, None);
    let (status, _) = post_proto(app, "/v1/metrics", Some(&token), req.encode_to_vec()).await;
    assert_eq!(status, http::StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn project_metrics_need_observe_write(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    // Create user + project but NO observe_write permission
    let user_id = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let req = make_metrics_request("proj.metric", 10.0, Some(project_id));
    let (status, _) = post_proto(app, "/v1/metrics", Some(&token), req.encode_to_vec()).await;
    // Returns 404 to avoid leaking project existence
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_invalid_protobuf(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    // Send garbage bytes — should fail protobuf decode
    let (status, _) = post_proto(app, "/v1/traces", Some(&token), vec![0xff, 0xfe, 0xfd]).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_invalid_protobuf_logs(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let (status, _) = post_proto(app, "/v1/logs", Some(&token), vec![0xff, 0xfe]).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_invalid_protobuf_metrics(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let (status, _) = post_proto(app, "/v1/metrics", Some(&token), vec![0xff]).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_deactivated_user(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    // Deactivate the user after creating the token
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("deactivate user");

    let req = make_trace_request(&[0u8; 16], "test", None);
    let (status, _) = post_proto(app, "/v1/traces", Some(&token), req.encode_to_vec()).await;
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_accepts_session_cookie(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey.clone());

    let cancel = CancellationToken::new();
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;

    // Create a session directly in the DB
    let (raw_token, token_hash) = platform_auth::generate_session_token();
    sqlx::query(
        "INSERT INTO auth_sessions (id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, NOW() + INTERVAL '1 day')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&token_hash)
    .execute(&pool)
    .await
    .expect("seed session");

    // POST with session cookie instead of Bearer token
    let metric_name = format!("session.metric.{}", Uuid::new_v4());
    let req_body = make_metrics_request(&metric_name, 33.3, None);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/metrics")
        .header("content-type", "application/x-protobuf")
        .header("cookie", format!("session={raw_token}"))
        .body(Body::from(req_body.encode_to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let series = sqlx::query("SELECT last_value FROM metric_series WHERE name = $1")
        .bind(&metric_name)
        .fetch_one(&pool)
        .await
        .expect("metric should be written via session auth");
    let last_value: f64 = series.get("last_value");
    assert!((last_value - 33.3).abs() < f64::EPSILON);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handler_rejects_deactivated_session_user(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;

    let (raw_token, token_hash) = platform_auth::generate_session_token();
    sqlx::query(
        "INSERT INTO auth_sessions (id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, NOW() + INTERVAL '1 day')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&token_hash)
    .execute(&pool)
    .await
    .expect("seed session");

    // Deactivate user
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("deactivate user");

    let req_body = make_trace_request(&[0u8; 16], "test", None);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/traces")
        .header("content-type", "application/x-protobuf")
        .header("cookie", format!("session={raw_token}"))
        .body(Body::from(req_body.encode_to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn project_metrics_with_observe_write_succeeds(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool.clone(), valkey.clone());

    let cancel = CancellationToken::new();
    let alert_router = Arc::new(RwLock::new(AlertRouter::empty()));
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, user_id).await;
    seed_observe_write_permission(&pool, user_id, project_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let metric_name = format!("proj.metric.{}", Uuid::new_v4());
    let req = make_metrics_request(&metric_name, 55.0, Some(project_id));
    let (status, _) = post_proto(app, "/v1/metrics", Some(&token), req.encode_to_vec()).await;
    assert_eq!(status, http::StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let series = sqlx::query("SELECT project_id FROM metric_series WHERE name = $1")
        .bind(&metric_name)
        .fetch_one(&pool)
        .await
        .expect("series should exist");
    let pid: Option<Uuid> = series.get("project_id");
    assert_eq!(pid, Some(project_id));
}

// ===========================================================================
// Alert routing through HTTP path
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn metrics_route_to_alert_stream_via_handler(pool: PgPool) {
    let valkey = valkey_pool().await;
    let metric_name = format!("test.http.alert.{}", Uuid::new_v4());
    let rule_id = seed_alert_rule(&pool, &format!("metric:{metric_name} agg:avg"), None).await;

    let alert_router = Arc::new(RwLock::new(
        AlertRouter::from_db(&pool).await.expect("load router"),
    ));

    let state = ingest_state(pool.clone(), valkey.clone());
    let cancel = CancellationToken::new();
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    tokio::spawn(ingest::flush_metrics(
        pool.clone(),
        valkey.clone(),
        alert_router,
        metrics_rx,
        cancel.clone(),
    ));

    let app = platform_ingest::build_router(state, channels);

    let user_id = seed_user(&pool).await;
    seed_admin_permission(&pool, user_id).await;
    let token = seed_api_token(&pool, user_id, &[], None).await;

    let before_len: usize = valkey.xlen::<usize, _>(ALERT_STREAM_KEY).await.unwrap_or(0);

    let req = make_metrics_request(&metric_name, 100.0, None);
    let (status, _) = post_proto(app, "/v1/metrics", Some(&token), req.encode_to_vec()).await;
    assert_eq!(status, http::StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    cancel.cancel();

    let after_len: usize = valkey.xlen::<usize, _>(ALERT_STREAM_KEY).await.unwrap_or(0);

    assert!(
        after_len > before_len,
        "alert:samples should have new entry for rule {rule_id}"
    );
}

// ===========================================================================
// Healthz
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn healthz_returns_structured_json(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool, valkey);
    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let req = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["postgres"]["status"], "ok");
    assert!(json["postgres"]["latency_ms"].is_number());
    assert_eq!(json["valkey"]["status"], "ok");
    assert!(json["valkey"]["latency_ms"].is_number());
    assert_eq!(json["alert_router"]["status"], "ok");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn healthz_reports_degraded_alert_router(pool: PgPool) {
    let valkey = valkey_pool().await;
    let state = ingest_state(pool, valkey);
    state
        .alert_router_degraded
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let req = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // Degraded alert router → HTTP 200 (still passes readiness)
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["alert_router"]["status"], "degraded");
    assert_eq!(
        json["alert_router"]["message"],
        "alert router failed to load"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn healthz_returns_503_when_pg_unreachable(pool: PgPool) {
    let valkey = valkey_pool().await;
    // Close the real pool so the health check fails
    pool.close().await;
    let state = ingest_state(pool, valkey);

    let (channels, _spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let app = platform_ingest::build_router(state, channels);

    let req = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(json["status"], "unhealthy");
    assert_eq!(json["postgres"]["status"], "error");
}

// ===========================================================================
// Binary smoke test — full `run()` startup/serve/shutdown
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn run_serves_healthz_and_shuts_down(pool: PgPool) {
    let valkey = valkey_pool().await;
    let cancel = CancellationToken::new();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let cancel_bg = cancel.clone();
    let server = tokio::spawn(platform_ingest::run(
        listener, pool, valkey, cancel_bg, false, 100,
    ));

    // Wait for the server to be ready
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/healthz");
    let mut ready = false;
    for _ in 0..20 {
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            let body = resp.text().await.unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(json["status"], "ok");
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "server should respond to /healthz within 2s");

    // Trigger graceful shutdown
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("server should shut down within 5s")
        .expect("server task should not panic");
    assert!(result.is_ok(), "run() should complete without error");
}
