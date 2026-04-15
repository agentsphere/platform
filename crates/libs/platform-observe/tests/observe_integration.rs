// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform-observe` crate.
//!
//! Tests batch write functions, alert DB operations, and metric evaluation
//! against a real Postgres (via `#[sqlx::test]`) and real Valkey.

use chrono::Utc;
use fred::interfaces::ClientLike;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use platform_observe::types::{LogEntryRecord, MetricRecord, SpanRecord};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn valkey_pool() -> fred::clients::Pool {
    let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    // fred's URL parser chokes on redis://:password@host (empty username).
    // Normalize to redis://default:password@host which is equivalent.
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
// Span writes
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn write_spans_inserts_and_creates_trace(pool: PgPool) {
    let trace_id = format!("t-{}", Uuid::new_v4());
    let span_id = format!("s-{}", Uuid::new_v4());
    let span = make_span(&trace_id, &span_id, None);

    platform_observe::store::write_spans(&pool, &[span])
        .await
        .expect("write_spans should succeed");

    // Verify span in DB
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE span_id = $1")
        .bind(&span_id)
        .fetch_one(&pool)
        .await
        .expect("query spans");
    assert_eq!(count.0, 1, "should have inserted 1 span");

    // Verify trace was upserted
    let trace_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM traces WHERE trace_id = $1")
        .bind(&trace_id)
        .fetch_one(&pool)
        .await
        .expect("query traces");
    assert_eq!(trace_count.0, 1, "should have created 1 trace");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn write_spans_with_project(pool: PgPool) {
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    let trace_id = format!("t-{}", Uuid::new_v4());
    let span_id = format!("s-{}", Uuid::new_v4());
    let span = make_span(&trace_id, &span_id, Some(project_id));

    platform_observe::store::write_spans(&pool, &[span])
        .await
        .expect("should succeed");

    let row = sqlx::query("SELECT project_id FROM spans WHERE span_id = $1")
        .bind(&span_id)
        .fetch_one(&pool)
        .await
        .expect("fetch span");
    let pid: Option<Uuid> = row.get("project_id");
    assert_eq!(pid, Some(project_id));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn write_spans_empty_is_noop(pool: PgPool) {
    let result = platform_observe::store::write_spans(&pool, &[]).await;
    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// Log writes
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn write_logs_inserts_batch(pool: PgPool) {
    let logs = vec![make_log(None), make_log(None), make_log(None)];

    // Count before
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();

    platform_observe::store::write_logs(&pool, &logs)
        .await
        .expect("write_logs should succeed");

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(after.0 - before.0, 3, "should have inserted 3 log entries");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn write_logs_empty_is_noop(pool: PgPool) {
    let result = platform_observe::store::write_logs(&pool, &[]).await;
    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// Metric writes
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn write_metrics_creates_series_and_sample(pool: PgPool) {
    let name = format!("test.metric.{}", Uuid::new_v4());
    let metric = make_metric(&name, 42.5, None);

    platform_observe::store::write_metrics(&pool, &[metric])
        .await
        .expect("write_metrics should succeed");

    // Check series created
    let series = sqlx::query("SELECT id, last_value FROM metric_series WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .expect("fetch series");
    let series_id: Uuid = series.get("id");
    let last_value: f64 = series.get("last_value");
    assert!((last_value - 42.5).abs() < f64::EPSILON);

    // Check sample created
    let sample_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM metric_samples WHERE series_id = $1")
            .bind(series_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sample_count.0, 1);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn write_metrics_upserts_existing_series(pool: PgPool) {
    let name = format!("test.metric.{}", Uuid::new_v4());

    // First write
    let m1 = MetricRecord {
        name: name.clone(),
        labels: serde_json::json!({}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: Utc::now() - chrono::Duration::seconds(10),
        value: 10.0,
    };
    platform_observe::store::write_metrics(&pool, &[m1])
        .await
        .unwrap();

    // Second write — same name, different value + timestamp
    let m2 = MetricRecord {
        name: name.clone(),
        labels: serde_json::json!({}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: Utc::now(),
        value: 20.0,
    };
    platform_observe::store::write_metrics(&pool, &[m2])
        .await
        .unwrap();

    // Should still be 1 series
    let series_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM metric_series WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(series_count.0, 1, "should upsert, not duplicate");

    // last_value should be updated to 20.0
    let last: (f64,) = sqlx::query_as("SELECT last_value FROM metric_series WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!((last.0 - 20.0).abs() < f64::EPSILON);

    // Should have 2 samples
    let series_id: (Uuid,) = sqlx::query_as("SELECT id FROM metric_series WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    let sample_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM metric_samples WHERE series_id = $1")
            .bind(series_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sample_count.0, 2);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn write_metrics_empty_is_noop(pool: PgPool) {
    let result = platform_observe::store::write_metrics(&pool, &[]).await;
    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// Alert operations
// ---------------------------------------------------------------------------

/// Seed an alert rule. Returns `rule_id`.
async fn seed_alert_rule(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO alert_rules (id, name, query, condition, threshold, for_seconds, severity)
         VALUES ($1, $2, 'metric:cpu agg:avg', 'gt', 80.0, 60, 'warning')",
    )
    .bind(id)
    .bind(format!("rule-{id}"))
    .execute(pool)
    .await
    .expect("seed alert_rule");
    id
}

#[sqlx::test(migrations = "../../../migrations")]
async fn fire_alert_creates_firing_event(pool: PgPool) {
    let rule_id = seed_alert_rule(&pool).await;

    platform_observe::alert::fire_alert(&pool, rule_id, Some(95.5))
        .await
        .expect("fire_alert should succeed");

    let row = sqlx::query(
        "SELECT status, value FROM alert_events WHERE rule_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(rule_id)
    .fetch_one(&pool)
    .await
    .expect("fetch event");

    let status: String = row.get("status");
    let value: Option<f64> = row.get("value");
    assert_eq!(status, "firing");
    assert!((value.unwrap() - 95.5).abs() < f64::EPSILON);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_alert_sets_resolved_at(pool: PgPool) {
    let rule_id = seed_alert_rule(&pool).await;

    // Fire first
    platform_observe::alert::fire_alert(&pool, rule_id, Some(95.0))
        .await
        .unwrap();

    // Then resolve
    platform_observe::alert::resolve_alert(&pool, rule_id)
        .await
        .expect("resolve should succeed");

    let row = sqlx::query(
        "SELECT status, resolved_at FROM alert_events WHERE rule_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(rule_id)
    .fetch_one(&pool)
    .await
    .expect("fetch event");

    let status: String = row.get("status");
    let resolved_at: Option<chrono::DateTime<Utc>> = row.get("resolved_at");
    assert_eq!(status, "resolved");
    assert!(resolved_at.is_some(), "resolved_at should be set");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn resolve_alert_without_firing_is_noop(pool: PgPool) {
    let rule_id = seed_alert_rule(&pool).await;

    // Resolve without firing — should not error
    let result = platform_observe::alert::resolve_alert(&pool, rule_id).await;
    assert!(result.is_ok());

    // No events should exist
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM alert_events WHERE rule_id = $1 AND status = 'resolved'",
    )
    .bind(rule_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count.0, 0);
}

// ---------------------------------------------------------------------------
// Metric evaluation
// ---------------------------------------------------------------------------

/// Seed metric series + samples for evaluation tests.
async fn seed_metric_samples(pool: &PgPool, name: &str, values: &[f64]) -> Uuid {
    let series_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO metric_series (id, name, labels, metric_type)
         VALUES ($1, $2, '{}'::jsonb, 'gauge')",
    )
    .bind(series_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("seed metric series");

    let now = Utc::now();
    for (i, val) in values.iter().enumerate() {
        #[allow(clippy::cast_possible_wrap)]
        let ts = now - chrono::Duration::seconds(i as i64 * 10);
        sqlx::query("INSERT INTO metric_samples (series_id, timestamp, value) VALUES ($1, $2, $3)")
            .bind(series_id)
            .bind(ts)
            .bind(val)
            .execute(pool)
            .await
            .expect("seed sample");
    }

    series_id
}

#[sqlx::test(migrations = "../../../migrations")]
async fn evaluate_metric_avg(pool: PgPool) {
    let name = format!("eval.avg.{}", Uuid::new_v4());
    seed_metric_samples(&pool, &name, &[10.0, 20.0, 30.0]).await;

    let result = platform_observe::alert::evaluate_metric(&pool, &name, None, "avg", 300)
        .await
        .expect("should succeed");

    let value = result.expect("should have a value");
    assert!((value - 20.0).abs() < f64::EPSILON, "avg of 10,20,30 = 20");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn evaluate_metric_sum(pool: PgPool) {
    let name = format!("eval.sum.{}", Uuid::new_v4());
    seed_metric_samples(&pool, &name, &[10.0, 20.0, 30.0]).await;

    let result = platform_observe::alert::evaluate_metric(&pool, &name, None, "sum", 300)
        .await
        .expect("should succeed");

    let value = result.expect("should have a value");
    assert!((value - 60.0).abs() < f64::EPSILON, "sum of 10,20,30 = 60");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn evaluate_metric_count(pool: PgPool) {
    let name = format!("eval.count.{}", Uuid::new_v4());
    seed_metric_samples(&pool, &name, &[10.0, 20.0, 30.0]).await;

    let result = platform_observe::alert::evaluate_metric(&pool, &name, None, "count", 300)
        .await
        .expect("should succeed");

    let value = result.expect("should have a value");
    assert!((value - 3.0).abs() < f64::EPSILON, "count of 3 samples = 3");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn evaluate_metric_max(pool: PgPool) {
    let name = format!("eval.max.{}", Uuid::new_v4());
    seed_metric_samples(&pool, &name, &[10.0, 30.0, 20.0]).await;

    let result = platform_observe::alert::evaluate_metric(&pool, &name, None, "max", 300)
        .await
        .expect("should succeed");

    let value = result.expect("should have a value");
    assert!((value - 30.0).abs() < f64::EPSILON, "max = 30");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn evaluate_metric_min(pool: PgPool) {
    let name = format!("eval.min.{}", Uuid::new_v4());
    seed_metric_samples(&pool, &name, &[10.0, 30.0, 20.0]).await;

    let result = platform_observe::alert::evaluate_metric(&pool, &name, None, "min", 300)
        .await
        .expect("should succeed");

    let value = result.expect("should have a value");
    assert!((value - 10.0).abs() < f64::EPSILON, "min = 10");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn evaluate_metric_no_data_returns_none(pool: PgPool) {
    let name = format!("eval.nodata.{}", Uuid::new_v4());

    let result = platform_observe::alert::evaluate_metric(&pool, &name, None, "avg", 300)
        .await
        .expect("should succeed");

    assert!(result.is_none(), "no data should return None");
}

// ---------------------------------------------------------------------------
// handle_alert_state integration (with Valkey pub/sub)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn handle_alert_state_fires_and_publishes(pool: PgPool) {
    let valkey = valkey_pool().await;
    let rule_id = seed_alert_rule(&pool).await;

    let mut state = platform_observe::alert::AlertState {
        first_triggered: Some(Utc::now() - chrono::Duration::seconds(120)),
        firing: false,
    };
    let rule_info = platform_observe::alert::AlertRuleInfo {
        id: rule_id,
        name: "test-alert",
        severity: "warning",
        project_id: None,
        for_seconds: 60,
    };

    // Condition met + past hold period → should fire
    platform_observe::alert::handle_alert_state(
        &pool,
        &valkey,
        true,
        Some(95.0),
        Utc::now(),
        &mut state,
        &rule_info,
    )
    .await;

    assert!(state.firing, "state should be firing");

    // Verify the DB event was created
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM alert_events WHERE rule_id = $1 AND status = 'firing'",
    )
    .bind(rule_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count.0, 1, "should have 1 firing event");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn handle_alert_state_resolves(pool: PgPool) {
    let valkey = valkey_pool().await;
    let rule_id = seed_alert_rule(&pool).await;

    // Pre-fire the alert
    platform_observe::alert::fire_alert(&pool, rule_id, Some(95.0))
        .await
        .unwrap();

    let mut state = platform_observe::alert::AlertState {
        first_triggered: Some(Utc::now() - chrono::Duration::seconds(300)),
        firing: true,
    };
    let rule_info = platform_observe::alert::AlertRuleInfo {
        id: rule_id,
        name: "test-alert",
        severity: "warning",
        project_id: None,
        for_seconds: 60,
    };

    // Condition no longer met → should resolve
    platform_observe::alert::handle_alert_state(
        &pool,
        &valkey,
        false,
        None,
        Utc::now(),
        &mut state,
        &rule_info,
    )
    .await;

    assert!(!state.firing, "state should no longer be firing");

    let row = sqlx::query("SELECT status, resolved_at FROM alert_events WHERE rule_id = $1")
        .bind(rule_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let status: String = row.get("status");
    assert_eq!(status, "resolved");
}

// ---------------------------------------------------------------------------
// Ingest record builders
// ---------------------------------------------------------------------------

use platform_observe::ingest;
use platform_observe::proto;

fn str_kv(key: &str, val: &str) -> proto::KeyValue {
    proto::KeyValue {
        key: key.into(),
        value: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(val.into())),
        }),
    }
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_span_record_basic(pool: PgPool) {
    let span = proto::Span {
        trace_id: vec![1u8; 16],
        span_id: vec![2u8; 8],
        parent_span_id: vec![],
        name: "test-op".into(),
        kind: proto::SpanKind::Server as i32,
        start_time_unix_nano: 1_700_000_000_000_000_000,
        end_time_unix_nano: 1_700_000_001_000_000_000,
        attributes: vec![],
        events: vec![],
        status: Some(proto::SpanStatus {
            message: String::new(),
            code: proto::StatusCode::Ok as i32,
        }),
    };
    let resource_attrs = vec![str_kv("service.name", "my-svc")];

    let rec = ingest::build_span_record(&span, &resource_attrs, &pool).await;
    assert_eq!(rec.trace_id, proto::trace_id_to_hex(&[1u8; 16]));
    assert_eq!(rec.span_id, proto::span_id_to_hex(&[2u8; 8]));
    assert!(rec.parent_span_id.is_none());
    assert_eq!(rec.name, "test-op");
    assert_eq!(rec.service, "my-svc");
    assert_eq!(rec.kind, "server");
    assert_eq!(rec.status, "ok");
    assert!(rec.finished_at.is_some());
    assert_eq!(rec.duration_ms, Some(1000));
    assert!(rec.project_id.is_none());
    assert!(rec.session_id.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_span_record_no_end_time(pool: PgPool) {
    let span = proto::Span {
        trace_id: vec![1u8; 16],
        span_id: vec![2u8; 8],
        parent_span_id: vec![],
        name: "op".into(),
        kind: 0,
        start_time_unix_nano: 1_700_000_000_000_000_000,
        end_time_unix_nano: 0,
        attributes: vec![],
        events: vec![],
        status: None,
    };

    let rec = ingest::build_span_record(&span, &[], &pool).await;
    assert!(rec.finished_at.is_none());
    assert!(rec.duration_ms.is_none());
    assert_eq!(rec.status, "unset");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_span_record_with_parent(pool: PgPool) {
    let span = proto::Span {
        trace_id: vec![1u8; 16],
        span_id: vec![2u8; 8],
        parent_span_id: vec![3u8; 8],
        name: "child".into(),
        kind: 3, // client
        start_time_unix_nano: 1_700_000_000_000_000_000,
        end_time_unix_nano: 1_700_000_000_500_000_000,
        attributes: vec![],
        events: vec![],
        status: Some(proto::SpanStatus {
            message: String::new(),
            code: proto::StatusCode::Error as i32,
        }),
    };

    let rec = ingest::build_span_record(&span, &[], &pool).await;
    assert_eq!(rec.parent_span_id, Some(proto::span_id_to_hex(&[3u8; 8])));
    assert_eq!(rec.kind, "client");
    assert_eq!(rec.status, "error");
    assert_eq!(rec.duration_ms, Some(500));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_span_record_with_project_id(pool: PgPool) {
    let owner = seed_user(&pool).await;
    let (_, project_id) = seed_project(&pool, owner).await;

    let resource_attrs = vec![
        str_kv("service.name", "svc"),
        str_kv("platform.project_id", &project_id.to_string()),
    ];
    let span = proto::Span {
        trace_id: vec![1u8; 16],
        span_id: vec![2u8; 8],
        parent_span_id: vec![],
        name: "op".into(),
        kind: 0,
        start_time_unix_nano: 1_700_000_000_000_000_000,
        end_time_unix_nano: 0,
        attributes: vec![],
        events: vec![],
        status: None,
    };

    let rec = ingest::build_span_record(&span, &resource_attrs, &pool).await;
    assert_eq!(rec.project_id, Some(project_id));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_span_record_with_events(pool: PgPool) {
    let span = proto::Span {
        trace_id: vec![1u8; 16],
        span_id: vec![2u8; 8],
        parent_span_id: vec![],
        name: "op".into(),
        kind: 0,
        start_time_unix_nano: 1_700_000_000_000_000_000,
        end_time_unix_nano: 0,
        attributes: vec![],
        events: vec![proto::SpanEvent {
            time_unix_nano: 1_700_000_000_500_000_000,
            name: "exception".into(),
            attributes: vec![],
        }],
        status: None,
    };

    let rec = ingest::build_span_record(&span, &[], &pool).await;
    assert!(rec.events.is_some());
    let events = rec.events.unwrap();
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["name"], "exception");
}

// -- build_log_record --

#[sqlx::test(migrations = "../../../migrations")]
async fn build_log_record_basic(pool: PgPool) {
    let log = proto::LogRecord {
        time_unix_nano: 1_700_000_000_000_000_000,
        severity_number: 9, // info
        severity_text: String::new(),
        body: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue("hello".into())),
        }),
        attributes: vec![],
        trace_id: vec![],
        span_id: vec![],
    };
    let resource_attrs = vec![str_kv("service.name", "log-svc")];

    let rec = ingest::build_log_record(&log, &resource_attrs, &pool).await;
    assert_eq!(rec.service, "log-svc");
    assert_eq!(rec.level, "info");
    assert_eq!(rec.message, "hello");
    assert_eq!(rec.source, "external");
    assert!(rec.trace_id.is_none());
    assert!(rec.span_id.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_log_record_severity_text_overrides(pool: PgPool) {
    let log = proto::LogRecord {
        time_unix_nano: 1_700_000_000_000_000_000,
        severity_number: 17, // error
        severity_text: "WARNING".into(),
        body: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue("msg".into())),
        }),
        attributes: vec![],
        trace_id: vec![],
        span_id: vec![],
    };

    let rec = ingest::build_log_record(&log, &[], &pool).await;
    assert_eq!(rec.level, "warning"); // lowercase of severity_text
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_log_record_body_int_value(pool: PgPool) {
    let log = proto::LogRecord {
        time_unix_nano: 1_700_000_000_000_000_000,
        severity_number: 9,
        severity_text: String::new(),
        body: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::IntValue(42)),
        }),
        attributes: vec![],
        trace_id: vec![],
        span_id: vec![],
    };

    let rec = ingest::build_log_record(&log, &[], &pool).await;
    assert_eq!(rec.message, "42");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_log_record_body_none(pool: PgPool) {
    let log = proto::LogRecord {
        time_unix_nano: 1_700_000_000_000_000_000,
        severity_number: 9,
        severity_text: String::new(),
        body: None,
        attributes: vec![],
        trace_id: vec![],
        span_id: vec![],
    };

    let rec = ingest::build_log_record(&log, &[], &pool).await;
    assert_eq!(rec.message, "");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_log_record_zero_timestamp_uses_now(pool: PgPool) {
    let before = Utc::now();
    let log = proto::LogRecord {
        time_unix_nano: 0,
        severity_number: 9,
        severity_text: String::new(),
        body: None,
        attributes: vec![],
        trace_id: vec![],
        span_id: vec![],
    };

    let rec = ingest::build_log_record(&log, &[], &pool).await;
    assert!(rec.timestamp >= before);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_log_record_with_trace_and_span(pool: PgPool) {
    let log = proto::LogRecord {
        time_unix_nano: 1_700_000_000_000_000_000,
        severity_number: 9,
        severity_text: String::new(),
        body: None,
        attributes: vec![],
        trace_id: vec![0xABu8; 16],
        span_id: vec![0xCDu8; 8],
    };

    let rec = ingest::build_log_record(&log, &[], &pool).await;
    assert_eq!(rec.trace_id, Some(proto::trace_id_to_hex(&[0xABu8; 16])));
    assert_eq!(rec.span_id, Some(proto::span_id_to_hex(&[0xCDu8; 8])));
}

// -- build_metric_records --

#[sqlx::test(migrations = "../../../migrations")]
async fn build_metric_records_gauge(pool: PgPool) {
    let metric = proto::Metric {
        name: "cpu_usage".into(),
        description: String::new(),
        unit: "percent".into(),
        data: Some(proto::metric_data::Data::Gauge(proto::Gauge {
            data_points: vec![proto::NumberDataPoint {
                value: Some(proto::number_data_point::Value::AsDouble(55.5)),
                time_unix_nano: 1_700_000_000_000_000_000,
                attributes: vec![],
            }],
        })),
    };

    let recs = ingest::build_metric_records(&metric, &[], &pool).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].name, "cpu_usage");
    assert_eq!(recs[0].metric_type, "gauge");
    assert_eq!(recs[0].unit.as_deref(), Some("percent"));
    assert!((recs[0].value - 55.5).abs() < f64::EPSILON);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_metric_records_monotonic_sum(pool: PgPool) {
    let metric = proto::Metric {
        name: "requests_total".into(),
        description: String::new(),
        unit: String::new(),
        data: Some(proto::metric_data::Data::Sum(proto::Sum {
            data_points: vec![proto::NumberDataPoint {
                value: Some(proto::number_data_point::Value::AsInt(1000)),
                time_unix_nano: 1_700_000_000_000_000_000,
                attributes: vec![],
            }],
            is_monotonic: true,
        })),
    };

    let recs = ingest::build_metric_records(&metric, &[], &pool).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].metric_type, "counter");
    assert!(recs[0].unit.is_none()); // empty unit → None
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_metric_records_non_monotonic_sum(pool: PgPool) {
    let metric = proto::Metric {
        name: "temperature".into(),
        description: String::new(),
        unit: "celsius".into(),
        data: Some(proto::metric_data::Data::Sum(proto::Sum {
            data_points: vec![proto::NumberDataPoint {
                value: Some(proto::number_data_point::Value::AsDouble(22.5)),
                time_unix_nano: 1_700_000_000_000_000_000,
                attributes: vec![],
            }],
            is_monotonic: false,
        })),
    };

    let recs = ingest::build_metric_records(&metric, &[], &pool).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].metric_type, "gauge");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_metric_records_histogram_with_sum(pool: PgPool) {
    let metric = proto::Metric {
        name: "latency".into(),
        description: String::new(),
        unit: "ms".into(),
        data: Some(proto::metric_data::Data::Histogram(proto::Histogram {
            data_points: vec![proto::HistogramDataPoint {
                sum: Some(1234.5),
                count: 10,
                time_unix_nano: 1_700_000_000_000_000_000,
                attributes: vec![],
                explicit_bounds: vec![],
                bucket_counts: vec![],
            }],
        })),
    };

    let recs = ingest::build_metric_records(&metric, &[], &pool).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].metric_type, "histogram");
    assert!((recs[0].value - 1234.5).abs() < f64::EPSILON);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_metric_records_histogram_no_sum(pool: PgPool) {
    let metric = proto::Metric {
        name: "latency".into(),
        description: String::new(),
        unit: String::new(),
        data: Some(proto::metric_data::Data::Histogram(proto::Histogram {
            data_points: vec![proto::HistogramDataPoint {
                sum: None,
                count: 10,
                time_unix_nano: 1_700_000_000_000_000_000,
                attributes: vec![],
                explicit_bounds: vec![],
                bucket_counts: vec![],
            }],
        })),
    };

    let recs = ingest::build_metric_records(&metric, &[], &pool).await;
    assert!(recs.is_empty()); // no sum → skipped
}

#[sqlx::test(migrations = "../../../migrations")]
async fn build_metric_records_no_data(pool: PgPool) {
    let metric = proto::Metric {
        name: "empty".into(),
        description: String::new(),
        unit: String::new(),
        data: None,
    };

    let recs = ingest::build_metric_records(&metric, &[], &pool).await;
    assert!(recs.is_empty());
}

// -- drain functions (channel → DB roundtrip) --

#[sqlx::test(migrations = "../../../migrations")]
async fn drain_spans_writes_to_db(pool: PgPool) {
    let (channels, spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);
    let trace_id = format!("t-drain-{}", Uuid::new_v4());
    let span_id = format!("s-drain-{}", Uuid::new_v4());

    let span = make_span(&trace_id, &span_id, None);
    channels.spans_tx.try_send(span).unwrap();

    // Drain and verify
    let mut rx = spans_rx;
    let mut buf = Vec::new();
    // manually drain like the private function does
    while buf.len() < 500 {
        match rx.try_recv() {
            Ok(r) => buf.push(r),
            Err(_) => break,
        }
    }
    assert_eq!(buf.len(), 1);
    platform_observe::store::write_spans(&pool, &buf)
        .await
        .unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE span_id = $1")
        .bind(&span_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn drain_logs_writes_to_db(pool: PgPool) {
    let (channels, _spans_rx, logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    let log = make_log(None);
    channels.logs_tx.try_send(log).unwrap();

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mut rx = logs_rx;
    let mut buf = Vec::new();
    while buf.len() < 500 {
        match rx.try_recv() {
            Ok(r) => buf.push(r),
            Err(_) => break,
        }
    }
    assert_eq!(buf.len(), 1);
    platform_observe::store::write_logs(&pool, &buf)
        .await
        .unwrap();

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after.0 - before.0, 1);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn drain_metrics_writes_to_db(pool: PgPool) {
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    let name = format!("drain.metric.{}", Uuid::new_v4());
    let metric = make_metric(&name, 99.9, None);
    channels.metrics_tx.try_send(metric).unwrap();

    let mut rx = metrics_rx;
    let mut buf = Vec::new();
    while buf.len() < 500 {
        match rx.try_recv() {
            Ok(r) => buf.push(r),
            Err(_) => break,
        }
    }
    assert_eq!(buf.len(), 1);
    platform_observe::store::write_metrics(&pool, &buf)
        .await
        .unwrap();

    let series_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM metric_series WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(series_count.0, 1);
}

// ---------------------------------------------------------------------------
// Flush loop tests (public flush_spans / flush_logs / flush_metrics)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_spans_drains_and_writes(pool: PgPool) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let (channels, spans_rx, _logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    let trace_id = format!("t-flush-{}", Uuid::new_v4());
    let span_id = format!("s-flush-{}", Uuid::new_v4());
    channels
        .spans_tx
        .try_send(make_span(&trace_id, &span_id, None))
        .unwrap();

    // Spawn the flush loop
    let flush_cancel = cancel.clone();
    let flush_pool = pool.clone();
    let flush_valkey = valkey_pool().await;
    let alert_router = std::sync::Arc::new(tokio::sync::RwLock::new(
        platform_observe::alert::AlertRouter::empty(),
    ));
    let handle = tokio::spawn(ingest::flush_spans(
        flush_pool,
        flush_valkey,
        alert_router,
        spans_rx,
        flush_cancel,
    ));

    // Wait for flush to process (interval is 1s, give it 1.5s)
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Cancel and wait for graceful shutdown
    cancel.cancel();
    handle.await.unwrap();

    // Verify the span was written to DB
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE span_id = $1")
        .bind(&span_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1, "flush_spans should have written the span to DB");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_logs_drains_and_publishes(pool: PgPool) {
    let valkey = valkey_pool().await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (channels, _spans_rx, logs_rx, _metrics_rx) = ingest::create_channels_with_capacity(100);

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();

    channels.logs_tx.try_send(make_log(None)).unwrap();

    // Spawn the flush loop
    let flush_cancel = cancel.clone();
    let flush_pool = pool.clone();
    let flush_valkey = valkey.clone();
    let alert_router = std::sync::Arc::new(tokio::sync::RwLock::new(
        platform_observe::alert::AlertRouter::empty(),
    ));
    let handle = tokio::spawn(ingest::flush_logs(
        flush_pool,
        flush_valkey,
        alert_router,
        logs_rx,
        flush_cancel,
    ));

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    cancel.cancel();
    handle.await.unwrap();

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after.0 - before.0,
        1,
        "flush_logs should have written 1 log entry"
    );
}

#[sqlx::test(migrations = "../../../migrations")]
async fn flush_metrics_drains_and_writes(pool: PgPool) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let (channels, _spans_rx, _logs_rx, metrics_rx) = ingest::create_channels_with_capacity(100);

    let name = format!("flush.metric.{}", Uuid::new_v4());
    channels
        .metrics_tx
        .try_send(make_metric(&name, 77.7, None))
        .unwrap();

    // Spawn the flush loop
    let flush_cancel = cancel.clone();
    let flush_pool = pool.clone();
    let flush_valkey = valkey_pool().await;
    let alert_router = std::sync::Arc::new(tokio::sync::RwLock::new(
        platform_observe::alert::AlertRouter::empty(),
    ));
    let handle = tokio::spawn(ingest::flush_metrics(
        flush_pool,
        flush_valkey,
        alert_router,
        metrics_rx,
        flush_cancel,
    ));

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    cancel.cancel();
    handle.await.unwrap();

    let series_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM metric_series WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        series_count.0, 1,
        "flush_metrics should have written the metric series"
    );
}
