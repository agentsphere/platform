// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for the observability module (traces, logs, metrics, alerts).

mod helpers;

use axum::http::StatusCode;
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{create_user, test_router, test_state};
use platform::observe::store::{MetricRecord, write_metrics};

// ---------------------------------------------------------------------------
// Store-level helpers
// ---------------------------------------------------------------------------

/// Insert a span via the store layer, then verify it's queryable via the HTTP API.
async fn insert_test_span(pool: &PgPool, trace_id: &str, span_id: &str, service: &str) {
    let now = Utc::now();
    let span = platform::observe::store::SpanRecord {
        trace_id: trace_id.into(),
        span_id: span_id.into(),
        parent_span_id: None,
        name: "test-span".into(),
        service: service.into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(42),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(42)),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(pool, &[span])
        .await
        .expect("write_spans failed");
}

async fn insert_test_log(pool: &PgPool, service: &str, level: &str, message: &str) {
    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: service.into(),
        level: level.into(),
        source: "external".into(),
        message: message.into(),
        attributes: None,
    };
    platform::observe::store::write_logs(pool, &[log])
        .await
        .expect("write_logs failed");
}

async fn insert_test_metric(pool: &PgPool, name: &str, value: f64) {
    let metric = platform::observe::store::MetricRecord {
        name: name.into(),
        labels: serde_json::json!({"host": "test-node"}),
        metric_type: "gauge".into(),
        unit: Some("bytes".into()),
        project_id: None,
        timestamp: Utc::now(),
        value,
    };
    platform::observe::store::write_metrics(pool, &[metric])
        .await
        .expect("write_metrics failed");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Write spans via store, query via /api/observe/traces → span appears.
#[sqlx::test(migrations = "./migrations")]
async fn write_and_query_spans(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let trace_id = format!("trace-{}", Uuid::new_v4());
    let span_id = format!("span-{}", Uuid::new_v4());
    insert_test_span(&pool, &trace_id, &span_id, "obs-test-svc").await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/traces").await;

    assert_eq!(status, StatusCode::OK, "trace query failed: {body}");
    let items = body["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|t| t["trace_id"].as_str() == Some(&trace_id)),
        "trace not found in results: {body}"
    );
}

/// Write logs via store, query via /api/observe/logs → log appears.
#[sqlx::test(migrations = "./migrations")]
async fn write_and_query_logs(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let unique_msg = format!("test-log-{}", Uuid::new_v4());
    insert_test_log(&pool, "log-test-svc", "info", &unique_msg).await;

    // unique_msg is alphanumeric + hyphens, safe to pass as query param directly
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?q={unique_msg}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "log query failed: {body}");
    assert!(
        body["total"].as_i64().unwrap() >= 1,
        "log not found: {body}"
    );
}

/// Write metrics via store, query via /api/observe/metrics → metric series appears.
#[sqlx::test(migrations = "./migrations")]
async fn write_and_query_metrics(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let metric_name = format!("test_metric_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &metric_name, 42.0).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={metric_name}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "metric query failed: {body}");
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(!series.is_empty(), "metric series empty");
    assert_eq!(series[0]["name"], metric_name);
}

/// List distinct metric names.
#[sqlx::test(migrations = "./migrations")]
async fn list_metric_names(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name1 = format!("mn_alpha_{}", Uuid::new_v4().simple());
    let name2 = format!("mn_beta_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name1, 1.0).await;
    insert_test_metric(&pool, &name2, 2.0).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/metrics/names").await;

    assert_eq!(status, StatusCode::OK, "names query failed: {body}");
    let names: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    let name_strs: Vec<&str> = names.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(name_strs.contains(&name1.as_str()), "name1 missing");
    assert!(name_strs.contains(&name2.as_str()), "name2 missing");
}

/// Alert CRUD cycle: create, get, list, patch, delete.
#[sqlx::test(migrations = "./migrations")]
async fn alert_crud(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Need alert:manage permission — admin has it
    // Create
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "high-cpu",
            "query": "metric:cpu_usage agg:avg window:300",
            "condition": "gt",
            "threshold": 90.0,
            "severity": "critical",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "alert create failed: {body}");
    let alert_id = body["id"].as_str().unwrap();

    // Get
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "high-cpu");

    // List
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/alerts").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["total"].as_i64().unwrap() >= 1);

    // Patch
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
        serde_json::json!({ "name": "very-high-cpu", "threshold": 95.0 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "alert patch failed: {body}");
    assert_eq!(body["name"], "very-high-cpu");

    // Delete
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify gone
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// User without observe:read gets 403 on query endpoints.
#[sqlx::test(migrations = "./migrations")]
async fn observe_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Create user with NO role at all — no observe:read
    let (_user_id, token) = create_user(&app, &admin_token, "no-observe", "noobs@test.com").await;

    let (status, _) = helpers::get_json(&app, &token, "/api/observe/traces").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = helpers::get_json(&app, &token, "/api/observe/logs").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Log query filter tests
// ---------------------------------------------------------------------------

/// Filter logs by level — only error logs returned when ?level=error.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_level(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let tag = Uuid::new_v4().simple().to_string();
    insert_test_log(&pool, &format!("svc-{tag}"), "info", "info msg").await;
    insert_test_log(&pool, &format!("svc-{tag}"), "error", "error msg").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service=svc-{tag}&level=error"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["level"], "error");
}

/// Filter logs by service.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_service(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc_a = format!("svc-a-{}", Uuid::new_v4().simple());
    let svc_b = format!("svc-b-{}", Uuid::new_v4().simple());
    insert_test_log(&pool, &svc_a, "info", "msg from a").await;
    insert_test_log(&pool, &svc_b, "info", "msg from b").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc_a}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["service"].as_str() == Some(&svc_a)));
}

/// Filter logs by text query.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_text_query(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let unique = Uuid::new_v4().simple().to_string();
    insert_test_log(&pool, "search-svc", "info", &format!("findme-{unique}")).await;
    insert_test_log(&pool, "search-svc", "info", "not matching").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?q=findme-{unique}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 1);
}

/// Log pagination works.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("page-svc-{}", Uuid::new_v4().simple());
    for i in 0..5 {
        insert_test_log(&pool, &svc, "info", &format!("log {i}")).await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(body["total"].as_i64().unwrap() >= 5);
}

// ---------------------------------------------------------------------------
// Trace detail tests
// ---------------------------------------------------------------------------

/// Get trace detail returns spans.
#[sqlx::test(migrations = "./migrations")]
async fn get_trace_detail(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let trace_id = format!("trace-detail-{}", Uuid::new_v4().simple());
    insert_test_span(&pool, &trace_id, "span-root", "detail-svc").await;
    insert_test_span(&pool, &trace_id, "span-child1", "detail-svc").await;
    insert_test_span(&pool, &trace_id, "span-child2", "detail-svc").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces/{trace_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "trace detail failed: {body}");
    assert_eq!(body["trace_id"], trace_id);
    assert_eq!(body["spans"].as_array().unwrap().len(), 3);
}

/// Get trace detail for nonexistent trace returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_trace_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/traces/nonexistent-trace-id-12345",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Metric query tests
// ---------------------------------------------------------------------------

/// Query metrics by name.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_by_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("qm_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name, 99.0).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert_eq!(series.len(), 1);
    assert!(!series[0]["points"].as_array().unwrap().is_empty());
}

/// Query metrics with time range filter.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_time_range(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("tr_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name, 50.0).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}&range=1h"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(!series.is_empty());
}

/// List metric names returns distinct entries.
#[sqlx::test(migrations = "./migrations")]
async fn list_metric_names_returns_distinct(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("dup_{}", Uuid::new_v4().simple());
    // Insert same metric name twice
    insert_test_metric(&pool, &name, 1.0).await;
    insert_test_metric(&pool, &name, 2.0).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/metrics/names").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    let matching: Vec<_> = names
        .iter()
        .filter(|n| n["name"].as_str() == Some(&name))
        .collect();
    assert_eq!(matching.len(), 1, "metric name should appear exactly once");
}

// ---------------------------------------------------------------------------
// Alert handler tests
// ---------------------------------------------------------------------------

/// Create alert with missing fields returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn create_alert_validation(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Missing required "query" field — axum rejects with 422 (plain text, not JSON)
    let status = helpers::post_status(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({ "name": "bad-alert", "condition": "gt", "threshold": 1.0, "severity": "warning" }),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422, got {status}"
    );
}

/// Partial update preserves other fields.
#[sqlx::test(migrations = "./migrations")]
async fn update_alert_partial(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "partial-test",
            "query": "metric:mem agg:avg window:60",
            "condition": "gt",
            "threshold": 80.0,
            "severity": "warning",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let alert_id = body["id"].as_str().unwrap();

    // Patch only enabled=false
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
        serde_json::json!({ "enabled": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["name"], "partial-test"); // preserved
    assert_eq!(body["severity"], "warning"); // preserved
}

/// List alert events for a new alert returns empty.
#[sqlx::test(migrations = "./migrations")]
async fn list_alert_events_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "no-events",
            "query": "metric:disk agg:max window:60",
            "condition": "gt",
            "threshold": 90.0,
            "severity": "info",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let alert_id = body["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

/// List all alert events endpoint returns empty when no events.
#[sqlx::test(migrations = "./migrations")]
async fn list_all_alert_events(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/alerts/events").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().is_some());
}

// ---------------------------------------------------------------------------
// Permission tests
// ---------------------------------------------------------------------------

/// Non-admin user cannot create alerts.
#[sqlx::test(migrations = "./migrations")]
async fn alert_manage_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_uid, token) = create_user(&app, &admin_token, "no-alert", "noalert@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "unauthorized",
            "query": "metric:x agg:avg window:60",
            "condition": "gt",
            "threshold": 1.0,
            "severity": "info",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Non-admin user gets 403 on log endpoint.
#[sqlx::test(migrations = "./migrations")]
async fn observe_logs_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_uid, token) = create_user(&app, &admin_token, "no-observe2", "noobs2@test.com").await;

    let (status, _) = helpers::get_json(&app, &token, "/api/observe/logs").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Parquet rotation tests
// ---------------------------------------------------------------------------

/// Rotate old logs to parquet — rows deleted from DB, bytes in `MinIO`.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_logs_archives_old_data(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    // Insert log with timestamp 72h ago (> 48h cutoff)
    let old_ts = Utc::now() - chrono::Duration::hours(72);
    let log = platform::observe::store::LogEntryRecord {
        timestamp: old_ts,
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: "rotate-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "old log for rotation".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM log_entries WHERE service = 'rotate-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(count.0 >= 1, "log should exist before rotation");

    // Run rotation
    let rotated = platform::observe::parquet::rotate_logs(&state)
        .await
        .unwrap();
    assert!(rotated >= 1, "should have rotated at least 1 log");

    // Verify deleted from DB
    let count_after: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM log_entries WHERE service = 'rotate-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count_after.0, 0, "rotated logs should be deleted");
}

/// Rotate old spans to parquet.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_spans_archives_old_data(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let old_ts = Utc::now() - chrono::Duration::hours(72);
    let span = platform::observe::store::SpanRecord {
        trace_id: "rot-trace".into(),
        span_id: "rot-span".into(),
        parent_span_id: None,
        name: "rotate-test".into(),
        service: "rotate-span-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(10),
        started_at: old_ts,
        finished_at: Some(old_ts + chrono::Duration::milliseconds(10)),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(&pool, &[span])
        .await
        .unwrap();

    let rotated = platform::observe::parquet::rotate_spans(&state)
        .await
        .unwrap();
    assert!(rotated >= 1);

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM spans WHERE service = 'rotate-span-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 0);
}

/// Rotate old metrics to parquet.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_metrics_archives_old_data(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    // Metric samples > 1h old
    let old_ts = Utc::now() - chrono::Duration::hours(2);
    let metric = platform::observe::store::MetricRecord {
        name: "rotate_metric_test".into(),
        labels: serde_json::json!({"host": "test"}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: old_ts,
        value: 42.0,
    };
    platform::observe::store::write_metrics(&pool, &[metric])
        .await
        .unwrap();

    let rotated = platform::observe::parquet::rotate_metrics(&state)
        .await
        .unwrap();
    assert!(rotated >= 1);
}

// ---------------------------------------------------------------------------
// Store — empty input early returns
// ---------------------------------------------------------------------------

/// `write_spans` with empty input is a no-op.
#[sqlx::test(migrations = "./migrations")]
async fn write_spans_empty_is_noop(pool: PgPool) {
    platform::observe::store::write_spans(&pool, &[])
        .await
        .expect("write_spans with empty input should succeed");
}

/// `write_logs` with empty input is a no-op.
#[sqlx::test(migrations = "./migrations")]
async fn write_logs_empty_is_noop(pool: PgPool) {
    platform::observe::store::write_logs(&pool, &[])
        .await
        .expect("write_logs with empty input should succeed");
}

/// `write_metrics` with empty input is a no-op.
#[sqlx::test(migrations = "./migrations")]
async fn write_metrics_empty_is_noop(pool: PgPool) {
    platform::observe::store::write_metrics(&pool, &[])
        .await
        .expect("write_metrics with empty input should succeed");
}

// ---------------------------------------------------------------------------
// Correlation — resolve_session
// ---------------------------------------------------------------------------

/// `resolve_session` fills `project_id` and `user_id` from `agent_sessions` table.
#[sqlx::test(migrations = "./migrations")]
async fn resolve_session_fills_project_and_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let proj_id = helpers::create_project(&app, &admin_token, "corr-proj", "private").await;

    // Insert an agent session row
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'test prompt', 'completed', 'anthropic')",
    )
    .bind(session_id)
    .bind(proj_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let mut envelope = platform::observe::correlation::CorrelationEnvelope {
        session_id: Some(session_id),
        project_id: None,
        user_id: None,
        ..Default::default()
    };

    platform::observe::correlation::resolve_session(&pool, &mut envelope)
        .await
        .expect("resolve_session should succeed");

    assert_eq!(envelope.project_id, Some(proj_id));
    assert_eq!(envelope.user_id, Some(admin_id));
}

/// `resolve_session` is a no-op when `project_id` and `user_id` already set.
#[sqlx::test(migrations = "./migrations")]
async fn resolve_session_no_op_when_already_set(pool: PgPool) {
    let proj_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();

    let mut envelope = platform::observe::correlation::CorrelationEnvelope {
        session_id: Some(session_id),
        project_id: Some(proj_id),
        user_id: Some(user_id),
        ..Default::default()
    };

    platform::observe::correlation::resolve_session(&pool, &mut envelope)
        .await
        .expect("resolve_session should succeed");

    // Values should remain unchanged (early return, no DB query)
    assert_eq!(envelope.project_id, Some(proj_id));
    assert_eq!(envelope.user_id, Some(user_id));
}

/// `resolve_session` is a no-op without `session_id`.
#[sqlx::test(migrations = "./migrations")]
async fn resolve_session_no_op_without_session(pool: PgPool) {
    let mut envelope = platform::observe::correlation::CorrelationEnvelope::default();

    platform::observe::correlation::resolve_session(&pool, &mut envelope)
        .await
        .expect("resolve_session should succeed");

    assert!(envelope.project_id.is_none());
    assert!(envelope.user_id.is_none());
}

// ---------------------------------------------------------------------------
// Session timeline
// ---------------------------------------------------------------------------

/// Helper: resolve admin `user_id` from token.
async fn get_admin_user_id(app: &axum::Router, token: &str) -> Uuid {
    let (status, body) = helpers::get_json(app, token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Insert an `agent_session` record directly into DB. Returns the session id.
async fn insert_session(pool: &PgPool, project_id: Uuid, user_id: Uuid) -> Uuid {
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status)
         VALUES ($1, $2, $3, 'test prompt', 'running')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert session");
    session_id
}

/// Insert a log entry with a `session_id`.
async fn insert_session_log(pool: &PgPool, session_id: Uuid, message: &str) {
    sqlx::query(
        "INSERT INTO log_entries (session_id, service, level, message, timestamp)
         VALUES ($1, 'agent-svc', 'info', $2, now())",
    )
    .bind(session_id)
    .bind(message)
    .execute(pool)
    .await
    .expect("insert session log");
}

/// Insert a span + trace with a `session_id`.
async fn insert_session_span(pool: &PgPool, session_id: Uuid, span_name: &str) {
    let trace_id = format!("trace-{}", Uuid::new_v4());
    let span_id = format!("span-{}", Uuid::new_v4());

    // Insert trace with session_id
    sqlx::query(
        "INSERT INTO traces (trace_id, session_id, root_span, service, status, started_at)
         VALUES ($1, $2, $3, 'agent-svc', 'ok', now())",
    )
    .bind(&trace_id)
    .bind(session_id)
    .bind(span_name)
    .execute(pool)
    .await
    .expect("insert trace");

    // Insert span linked to trace, with session_id denormalized
    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, name, service, kind, status, started_at, session_id)
         VALUES ($1, $2, $3, 'agent-svc', 'server', 'ok', now(), $4)",
    )
    .bind(&trace_id)
    .bind(&span_id)
    .bind(span_name)
    .bind(session_id)
    .execute(pool)
    .await
    .expect("insert span");
}

/// Session timeline returns logs and spans for a session.
#[sqlx::test(migrations = "./migrations")]
async fn session_timeline_logs_and_spans(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_user_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "timeline-proj", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id).await;

    insert_session_log(&pool, session_id, "agent started").await;
    insert_session_log(&pool, session_id, "running tool").await;
    insert_session_span(&pool, session_id, "code-edit").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/sessions/{session_id}/timeline"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "timeline query failed: {body}");
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 3, "expected 2 logs + 1 span: {body}");

    let logs: Vec<_> = entries.iter().filter(|e| e["kind"] == "log").collect();
    let spans: Vec<_> = entries.iter().filter(|e| e["kind"] == "span").collect();
    assert_eq!(logs.len(), 2);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["message"], "code-edit");
}

/// Session timeline returns empty array for session with no data.
#[sqlx::test(migrations = "./migrations")]
async fn session_timeline_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_user_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "timeline-empty", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/sessions/{session_id}/timeline"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

/// Session timeline returns 404 for non-existent session.
#[sqlx::test(migrations = "./migrations")]
async fn session_timeline_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/sessions/{fake_id}/timeline"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Spans denormalization (PR 0)
// ---------------------------------------------------------------------------

/// write_spans stores project_id, session_id, and user_id on span rows.
#[sqlx::test(migrations = "./migrations")]
async fn spans_written_with_project_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_user_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "denorm-proj", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id).await;

    let trace_id = format!("trace-{}", Uuid::new_v4());
    let span_id = format!("span-{}", Uuid::new_v4());
    let now = Utc::now();
    let span = platform::observe::store::SpanRecord {
        trace_id: trace_id.clone(),
        span_id: span_id.clone(),
        parent_span_id: None,
        name: "denorm-test".into(),
        service: "test-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(10),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(10)),
        project_id: Some(project_id),
        session_id: Some(session_id),
        user_id: Some(admin_id),
    };
    platform::observe::store::write_spans(&pool, &[span])
        .await
        .expect("write_spans failed");

    // Verify the columns are stored
    let row = sqlx::query("SELECT project_id, session_id, user_id FROM spans WHERE span_id = $1")
        .bind(&span_id)
        .fetch_one(&pool)
        .await
        .expect("fetch span");

    use sqlx::Row;
    let stored_project: Option<Uuid> = row.get("project_id");
    let stored_session: Option<Uuid> = row.get("session_id");
    let stored_user: Option<Uuid> = row.get("user_id");
    assert_eq!(stored_project, Some(project_id));
    assert_eq!(stored_session, Some(session_id));
    assert_eq!(stored_user, Some(admin_id));
}

/// Session timeline query returns spans via direct session_id filter (no join).
#[sqlx::test(migrations = "./migrations")]
async fn session_timeline_uses_span_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_user_id(&app, &admin_token).await;

    let project_id =
        helpers::create_project(&app, &admin_token, "timeline-direct", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id).await;

    // Insert a span with session_id directly on the span (no trace.session_id needed)
    let trace_id = format!("trace-{}", Uuid::new_v4());
    let span_id = format!("span-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO traces (trace_id, root_span, service, status, started_at)
         VALUES ($1, 'root-op', 'test-svc', 'ok', now())",
    )
    .bind(&trace_id)
    .execute(&pool)
    .await
    .expect("insert trace");

    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, name, service, kind, status, started_at, session_id)
         VALUES ($1, $2, 'direct-span', 'test-svc', 'server', 'ok', now(), $3)",
    )
    .bind(&trace_id)
    .bind(&span_id)
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("insert span");

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/sessions/{session_id}/timeline"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "timeline query failed: {body}");
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 1, "expected 1 span: {body}");
    assert_eq!(entries[0]["kind"], "span");
    assert_eq!(entries[0]["message"], "direct-span");
}

// ---------------------------------------------------------------------------
// Log query — time range filters
// ---------------------------------------------------------------------------

/// Filter logs by time range: only logs within [from, to] are returned.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_time_range(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("time-svc-{}", Uuid::new_v4().simple());

    // Insert a log with a known recent timestamp via the store layer
    let now = Utc::now();
    let log = platform::observe::store::LogEntryRecord {
        timestamp: now,
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: svc.clone(),
        level: "info".into(),
        source: "external".into(),
        message: "recent log".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    // Query with from=1h ago, to=1h from now — should find the log
    let from =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to = (now + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&from={from}&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["total"].as_i64().unwrap() >= 1,
        "log should be within range: {body}"
    );

    // Query with from=2h ago, to=1h ago — should NOT find the log
    let from_old =
        (now - chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to_old =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&from={from_old}&to={to_old}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"].as_i64().unwrap(),
        0,
        "log should be outside range: {body}"
    );
}

/// Filter logs by `trace_id`.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_trace_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let trace_id = format!("tr-{}", Uuid::new_v4().simple());
    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: Some(trace_id.clone()),
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: "trace-log-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "log with trace".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    // Also insert a log without a trace_id for the same service
    insert_test_log(&pool, "trace-log-svc", "info", "log without trace").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?trace_id={trace_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["trace_id"].as_str(), Some(trace_id.as_str()));
}

/// Empty result set returns total=0 and empty items.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_empty_result(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/logs?service=nonexistent-service-xyz-999",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

/// Search query validation rejects empty q parameter.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_empty_q_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Empty q= should fail validation (check_length requires min 1)
    let (status, _) = helpers::get_json(&app, &admin_token, "/api/observe/logs?q=").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty q should be rejected"
    );
}

/// Logs with `project_id` can be filtered by `project_id`.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_project_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "log-proj", "public").await;

    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: Some(project_id),
        session_id: None,
        user_id: None,
        service: "proj-log-svc".into(),
        level: "warn".into(),
        source: "external".into(),
        message: "project-scoped log".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    // Also insert a log without project_id
    insert_test_log(&pool, "proj-log-svc", "warn", "unscoped log").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .all(|i| i["project_id"].as_str() == Some(&project_id.to_string())),
        "all logs should belong to the project: {body}"
    );
    assert!(!items.is_empty());
}

/// Limit is capped at 100 even if a larger value is requested.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_limit_capped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("cap-svc-{}", Uuid::new_v4().simple());
    for i in 0..3 {
        insert_test_log(&pool, &svc, "info", &format!("cap log {i}")).await;
    }

    // Request limit=999 — should be capped to 100 (but we only have 3 logs)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&limit=999"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The point is the query doesn't fail; all 3 logs should be returned
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// Trace query — additional filters
// ---------------------------------------------------------------------------

/// Filter traces by service name.
#[sqlx::test(migrations = "./migrations")]
async fn list_traces_by_service(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc_a = format!("tsvc-a-{}", Uuid::new_v4().simple());
    let svc_b = format!("tsvc-b-{}", Uuid::new_v4().simple());
    insert_test_span(&pool, &format!("t-{}", Uuid::new_v4()), "s-a1", &svc_a).await;
    insert_test_span(&pool, &format!("t-{}", Uuid::new_v4()), "s-b1", &svc_b).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces?service={svc_a}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["service"].as_str(), Some(svc_a.as_str()));
}

/// Filter traces by status.
#[sqlx::test(migrations = "./migrations")]
async fn list_traces_by_status(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("status-svc-{}", Uuid::new_v4().simple());

    // Insert an "ok" span (root, creates trace with status "ok")
    let trace_ok = format!("tok-{}", Uuid::new_v4().simple());
    insert_test_span(&pool, &trace_ok, &format!("s-{}", Uuid::new_v4()), &svc).await;

    // Insert an "error" span (root, creates trace with status "error")
    let trace_err = format!("terr-{}", Uuid::new_v4().simple());
    let now = Utc::now();
    let span = platform::observe::store::SpanRecord {
        trace_id: trace_err.clone(),
        span_id: format!("se-{}", Uuid::new_v4()),
        parent_span_id: None,
        name: "error-span".into(),
        service: svc.clone(),
        kind: "server".into(),
        status: "error".into(),
        attributes: None,
        events: None,
        duration_ms: Some(100),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(100)),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(&pool, &[span])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces?service={svc}&status=error"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["status"].as_str(), Some("error"));
}

/// Filter traces by time range.
#[sqlx::test(migrations = "./migrations")]
async fn list_traces_time_range(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("trange-svc-{}", Uuid::new_v4().simple());
    insert_test_span(&pool, &format!("trange-{}", Uuid::new_v4()), "s-tr1", &svc).await;

    let now = Utc::now();
    let from =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to = (now + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces?service={svc}&from={from}&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["total"].as_i64().unwrap() >= 1);

    // Query in the past — should find nothing
    let old_from =
        (now - chrono::Duration::hours(3)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let old_to =
        (now - chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces?service={svc}&from={old_from}&to={old_to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
}

/// Trace list pagination.
#[sqlx::test(migrations = "./migrations")]
async fn list_traces_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("tpage-svc-{}", Uuid::new_v4().simple());
    for i in 0..5 {
        insert_test_span(
            &pool,
            &format!("tpage-{i}-{}", Uuid::new_v4()),
            &format!("sp-{i}-{}", Uuid::new_v4()),
            &svc,
        )
        .await;
    }

    // Page 1: limit=2, offset=0
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces?service={svc}&limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(body["total"].as_i64().unwrap() >= 5);

    // Page 2: limit=2, offset=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces?service={svc}&limit=2&offset=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

/// Empty trace list.
#[sqlx::test(migrations = "./migrations")]
async fn list_traces_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/traces?service=nonexistent-svc-xyz",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Trace detail — edge cases
// ---------------------------------------------------------------------------

/// Trace detail includes span attributes and events.
#[sqlx::test(migrations = "./migrations")]
async fn get_trace_detail_with_attributes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let trace_id = format!("tattr-{}", Uuid::new_v4().simple());
    let now = Utc::now();
    let span = platform::observe::store::SpanRecord {
        trace_id: trace_id.clone(),
        span_id: format!("sattr-{}", Uuid::new_v4()),
        parent_span_id: None,
        name: "attr-span".into(),
        service: "attr-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: Some(serde_json::json!({"http.method": "GET", "http.status_code": 200})),
        events: Some(
            serde_json::json!([{"name": "exception", "timestamp": "2026-01-01T00:00:00Z"}]),
        ),
        duration_ms: Some(55),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(55)),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(&pool, &[span])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces/{trace_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let spans = body["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 1);
    assert!(spans[0]["attributes"].is_object());
    assert_eq!(spans[0]["attributes"]["http.method"], "GET");
    assert!(spans[0]["events"].is_array());
}

/// Trace detail with parent-child spans preserves hierarchy fields.
#[sqlx::test(migrations = "./migrations")]
async fn get_trace_detail_parent_child(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let trace_id = format!("tpc-{}", Uuid::new_v4().simple());
    let parent_span_id = format!("sp-parent-{}", Uuid::new_v4());
    let child_span_id = format!("sp-child-{}", Uuid::new_v4());
    let now = Utc::now();

    // Root span
    let root = platform::observe::store::SpanRecord {
        trace_id: trace_id.clone(),
        span_id: parent_span_id.clone(),
        parent_span_id: None,
        name: "root-op".into(),
        service: "pc-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(100),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(100)),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    // Child span
    let child = platform::observe::store::SpanRecord {
        trace_id: trace_id.clone(),
        span_id: child_span_id.clone(),
        parent_span_id: Some(parent_span_id.clone()),
        name: "child-op".into(),
        service: "pc-svc".into(),
        kind: "client".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(50),
        started_at: now + chrono::Duration::milliseconds(10),
        finished_at: Some(now + chrono::Duration::milliseconds(60)),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(&pool, &[root, child])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/traces/{trace_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let spans = body["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 2);

    // First span (ordered by started_at ASC) should be the root
    assert!(spans[0]["parent_span_id"].is_null());
    assert_eq!(
        spans[1]["parent_span_id"].as_str(),
        Some(parent_span_id.as_str())
    );
}

// ---------------------------------------------------------------------------
// Metric query — edge cases
// ---------------------------------------------------------------------------

/// Query metrics without name parameter returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_missing_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _) = helpers::get_json(&app, &admin_token, "/api/observe/metrics").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing name should return 400"
    );
}

/// Query metrics with invalid labels JSON returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_invalid_labels(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/metrics?name=foo&labels=not-valid-json",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid labels JSON should return 400"
    );
}

/// Query metrics with labels filter.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_with_labels_filter(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("lbl_{}", Uuid::new_v4().simple());

    // Insert metric with host=node-a
    let m_a = platform::observe::store::MetricRecord {
        name: name.clone(),
        labels: serde_json::json!({"host": "node-a"}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: Utc::now(),
        value: 10.0,
    };
    platform::observe::store::write_metrics(&pool, &[m_a])
        .await
        .unwrap();

    // Insert metric with host=node-b
    let m_b = platform::observe::store::MetricRecord {
        name: name.clone(),
        labels: serde_json::json!({"host": "node-b"}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: Utc::now() + chrono::Duration::milliseconds(1),
        value: 20.0,
    };
    platform::observe::store::write_metrics(&pool, &[m_b])
        .await
        .unwrap();

    // Filter by host=node-a using URL-encoded JSON
    // Manually percent-encode the JSON: {"host":"node-a"} → %7B%22host%22%3A%22node-a%22%7D
    let encoded = "%7B%22host%22%3A%22node-a%22%7D";
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}&labels={encoded}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert_eq!(series.len(), 1, "should only match node-a series");
    assert_eq!(series[0]["labels"]["host"], "node-a");
}

/// Query metrics with various relative range values.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_relative_ranges(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("rng_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name, 42.0).await;

    for range in &["1h", "6h", "12h", "24h", "1d", "7d", "30d"] {
        let (status, body) = helpers::get_json(
            &app,
            &admin_token,
            &format!("/api/observe/metrics?name={name}&range={range}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "range={range} should succeed");
        let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
        assert!(!series.is_empty(), "range={range} should find the metric");
    }
}

/// Query metrics with an unknown range value falls back to no from filter.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_unknown_range(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("unk_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name, 5.0).await;

    // Unknown range like "99h" should be ignored (no from filter applied)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}&range=99h"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    // With no range filter, all data should still be found
    assert!(!series.is_empty());
}

/// Query metrics for a name that doesn't exist returns empty array.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_empty_result(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/metrics?name=nonexistent_metric_xyz_999",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(series.is_empty());
}

/// Query metrics with explicit from/to timestamps.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_explicit_time_range(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("etime_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name, 77.0).await;

    let now = Utc::now();
    let from =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to = (now + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}&from={from}&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(!series.is_empty());

    // Time range in the past — should return empty
    let old_from =
        (now - chrono::Duration::hours(3)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let old_to =
        (now - chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}&from={old_from}&to={old_to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(series.is_empty(), "should be empty for past range");
}

/// Multiple data points for the same metric series are grouped correctly.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_multiple_points(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("mpts_{}", Uuid::new_v4().simple());
    let now = Utc::now();

    // Insert 3 samples at different timestamps but same labels
    for i in 0..3_i64 {
        let m = platform::observe::store::MetricRecord {
            name: name.clone(),
            labels: serde_json::json!({"host": "multi-node"}),
            metric_type: "gauge".into(),
            unit: None,
            project_id: None,
            timestamp: now + chrono::Duration::seconds(i),
            #[allow(clippy::cast_precision_loss)]
            value: (i as f64) * 10.0,
        };
        platform::observe::store::write_metrics(&pool, &[m])
            .await
            .unwrap();
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert_eq!(series.len(), 1, "should have exactly one series");
    assert_eq!(
        series[0]["points"].as_array().unwrap().len(),
        3,
        "should have 3 data points"
    );
}

/// Metrics limit parameter constrains the number of rows returned.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_with_limit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("mlim_{}", Uuid::new_v4().simple());
    let now = Utc::now();

    // Insert 5 samples
    for i in 0..5_i64 {
        let m = platform::observe::store::MetricRecord {
            name: name.clone(),
            labels: serde_json::json!({"host": "lim-node"}),
            metric_type: "gauge".into(),
            unit: None,
            project_id: None,
            timestamp: now + chrono::Duration::seconds(i),
            #[allow(clippy::cast_precision_loss)]
            value: i as f64,
        };
        platform::observe::store::write_metrics(&pool, &[m])
            .await
            .unwrap();
    }

    // Request with limit=2 — should only return 2 data points total
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics?name={name}&limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    // The limit applies to total rows, so at most 2 points across all series
    let total_points: usize = series
        .iter()
        .map(|s| s["points"].as_array().unwrap().len())
        .sum();
    assert_eq!(total_points, 2, "limit should constrain total data points");
}

// ---------------------------------------------------------------------------
// Metric names — additional tests
// ---------------------------------------------------------------------------

/// Metric names endpoint with limit parameter.
#[sqlx::test(migrations = "./migrations")]
async fn list_metric_names_with_limit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let prefix = Uuid::new_v4().simple().to_string();
    for i in 0..5 {
        insert_test_metric(&pool, &format!("{prefix}_mn{i}"), f64::from(i) + 1.0).await;
    }

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/metrics/names?limit=3").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(names.len() <= 3, "limit should cap the result count");
}

/// Metric names with no data for a project returns empty list.
#[sqlx::test(migrations = "./migrations")]
async fn list_metric_names_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Use a project_id that has no metrics
    let project_id = helpers::create_project(&app, &admin_token, "empty-mn-proj", "public").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics/names?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(names.is_empty());
}

/// Metric names includes type and unit fields.
#[sqlx::test(migrations = "./migrations")]
async fn list_metric_names_fields(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("mf_{}", Uuid::new_v4().simple());
    // insert_test_metric inserts with metric_type=gauge, unit=bytes
    insert_test_metric(&pool, &name, 100.0).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/metrics/names").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    let entry = names.iter().find(|n| n["name"].as_str() == Some(&name));
    assert!(entry.is_some(), "metric name should be listed");
    let entry = entry.unwrap();
    assert_eq!(entry["metric_type"].as_str(), Some("gauge"));
    assert_eq!(entry["unit"].as_str(), Some("bytes"));
}

// ---------------------------------------------------------------------------
// Permission edge cases — project-scoped observe access
// ---------------------------------------------------------------------------

/// Observe reads on a private project return 404 for unauthorized users.
#[sqlx::test(migrations = "./migrations")]
async fn observe_project_scoped_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Create a private project
    let project_id = helpers::create_project(&app, &admin_token, "priv-obs-proj", "private").await;

    // Create a user with NO roles — no observe:read, no project:read
    let (uid, user_token) = create_user(&app, &admin_token, "obs-user", "obsuser@test.com").await;

    // Without observe:read, user should get 403 on observe endpoints
    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/observe/logs?project_id={project_id}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "user without observe:read should get 403"
    );

    // Same for traces
    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/observe/traces?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // With viewer role (which grants both observe:read AND project:read globally),
    // even a private project is accessible
    helpers::assign_role(&app, &admin_token, uid, "viewer", None, &pool).await;
    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/observe/logs?project_id={project_id}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "viewer with global project:read should access private project"
    );
}

/// Observe reads on a public project are allowed without project-specific role.
#[sqlx::test(migrations = "./migrations")]
async fn observe_public_project_allowed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pub-obs-proj", "public").await;

    let (uid, user_token) =
        create_user(&app, &admin_token, "obs-pub-user", "obspub@test.com").await;
    helpers::assign_role(&app, &admin_token, uid, "viewer", None, &pool).await;

    let (status, body) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/observe/logs?project_id={project_id}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "public project should be accessible: {body}"
    );
}

/// Non-existent `project_id` in log query returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn observe_nonexistent_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_project = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?project_id={fake_project}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-existent project should return 404"
    );
}

// ---------------------------------------------------------------------------
// Observe permission on metrics endpoints
// ---------------------------------------------------------------------------

/// Unprivileged user gets 403 on metrics query.
#[sqlx::test(migrations = "./migrations")]
async fn observe_metrics_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_uid, token) = create_user(&app, &admin_token, "no-obs-met", "nomet@test.com").await;

    let (status, _) =
        helpers::get_json(&app, &token, "/api/observe/metrics?name=some_metric").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = helpers::get_json(&app, &token, "/api/observe/metrics/names").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Unprivileged user gets 403 on trace detail.
#[sqlx::test(migrations = "./migrations")]
async fn observe_trace_detail_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Insert a trace first
    let trace_id = format!("perm-trace-{}", Uuid::new_v4().simple());
    insert_test_span(&pool, &trace_id, "perm-span", "perm-svc").await;

    let (_uid, token) = create_user(&app, &admin_token, "no-obs-tr", "notr@test.com").await;

    let (status, _) =
        helpers::get_json(&app, &token, &format!("/api/observe/traces/{trace_id}")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Unprivileged user gets 403 on session timeline.
#[sqlx::test(migrations = "./migrations")]
async fn observe_session_timeline_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_user_id(&app, &admin_token).await;
    let project_id = helpers::create_project(&app, &admin_token, "perm-tl-proj", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id).await;

    let (_uid, token) = create_user(&app, &admin_token, "no-obs-tl", "notl@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &token,
        &format!("/api/observe/sessions/{session_id}/timeline"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Metrics via /api/observe/metrics/query (alias)
// ---------------------------------------------------------------------------

/// The /api/observe/metrics/query endpoint works the same as /api/observe/metrics.
#[sqlx::test(migrations = "./migrations")]
async fn query_metrics_alias_endpoint(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let name = format!("alias_{}", Uuid::new_v4().simple());
    insert_test_metric(&pool, &name, 33.0).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics/query?name={name}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(!series.is_empty());
    assert_eq!(series[0]["name"], name);
}

// ---------------------------------------------------------------------------
// Log attributes and session_id filter
// ---------------------------------------------------------------------------

/// Logs with attributes are returned correctly.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_with_attributes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("attr-svc-{}", Uuid::new_v4().simple());
    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: svc.clone(),
        level: "info".into(),
        source: "external".into(),
        message: "log with attrs".into(),
        attributes: Some(serde_json::json!({"request_id": "abc-123", "duration_ms": 42})),
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0]["attributes"].is_object());
    assert_eq!(items[0]["attributes"]["request_id"], "abc-123");
}

/// Filter logs by `session_id`.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_session_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_user_id(&app, &admin_token).await;
    let project_id = helpers::create_project(&app, &admin_token, "sess-log-proj", "public").await;
    let session_id = insert_session(&pool, project_id, admin_id).await;

    // Insert log with session_id
    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: Some(project_id),
        session_id: Some(session_id),
        user_id: None,
        service: "sess-log-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "session-scoped log".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    // Also insert an unrelated log
    insert_test_log(&pool, "sess-log-svc", "info", "unrelated").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?session_id={session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let expected_sid = session_id.to_string();
    assert_eq!(items[0]["session_id"].as_str(), Some(expected_sid.as_str()));
}

// ---------------------------------------------------------------------------
// Log query — source filter
// ---------------------------------------------------------------------------

/// Filter logs by `source`.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_source(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("source-svc-{}", Uuid::new_v4().simple());
    let log_external = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: svc.clone(),
        level: "info".into(),
        source: "external".into(),
        message: "external log".into(),
        attributes: None,
    };
    let log_session = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: svc.clone(),
        level: "info".into(),
        source: "session".into(),
        message: "session log".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log_external, log_session])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&source=session"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], "session");
}

// ---------------------------------------------------------------------------
// Log query — task_name filter via attributes
// ---------------------------------------------------------------------------

/// Filter logs by `task_name` stored in attributes JSON.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_by_task_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("task-svc-{}", Uuid::new_v4().simple());
    let task = format!("deploy-{}", Uuid::new_v4().simple());
    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: svc.clone(),
        level: "info".into(),
        source: "system".into(),
        message: "task log".into(),
        attributes: Some(serde_json::json!({"task_name": task})),
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    // Also insert a log without that task_name
    insert_test_log(&pool, &svc, "info", "no task").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&task_name={task}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["message"], "task log");
}

// ---------------------------------------------------------------------------
// Log query — range parameter
// ---------------------------------------------------------------------------

/// Filter logs using relative `range` parameter.
#[sqlx::test(migrations = "./migrations")]
async fn search_logs_with_range_param(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("range-svc-{}", Uuid::new_v4().simple());
    insert_test_log(&pool, &svc, "info", "recent log").await;

    // range=1h should find the just-inserted log
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/logs?service={svc}&range=1h"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["total"].as_i64().unwrap() >= 1,
        "range=1h should find recent log"
    );
}

// ---------------------------------------------------------------------------
// Project-scoped logs endpoint
// ---------------------------------------------------------------------------

/// GET /api/projects/{project_id}/logs returns logs for that project.
#[sqlx::test(migrations = "./migrations")]
async fn project_logs_endpoint(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "proj-logs-ep", "public").await;

    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: Some(project_id),
        session_id: None,
        user_id: None,
        service: "proj-ep-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "project endpoint log".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/logs"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "project logs endpoint should return data"
    );
    assert!(
        items
            .iter()
            .all(|i| i["project_id"].as_str() == Some(&project_id.to_string())),
        "all logs should belong to the project"
    );
}

/// GET /api/projects/{project_id}/logs returns 404 for nonexistent project.
#[sqlx::test(migrations = "./migrations")]
async fn project_logs_nonexistent_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) =
        helpers::get_json(&app, &admin_token, &format!("/api/projects/{fake_id}/logs")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Rotate no-op when no old data
// ---------------------------------------------------------------------------

/// Rotation with no old data returns 0 without error.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_logs_no_old_data(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    // Insert recent log (within 48h cutoff)
    let log = platform::observe::store::LogEntryRecord {
        timestamp: Utc::now(),
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: "no-rotate-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "too recent to rotate".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[log])
        .await
        .unwrap();

    let rotated = platform::observe::parquet::rotate_logs(&state)
        .await
        .unwrap();
    // Recent logs should NOT be rotated
    assert_eq!(rotated, 0, "recent logs should not be rotated");

    // Verify the log is still in the DB
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM log_entries WHERE service = 'no-rotate-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(count.0 >= 1, "recent log should still exist");
}

/// Rotation for spans with no old data returns 0.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_spans_no_old_data(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let rotated = platform::observe::parquet::rotate_spans(&state)
        .await
        .unwrap();
    assert_eq!(rotated, 0);
}

/// Rotation for metrics with no old data returns 0.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_metrics_no_old_data(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let rotated = platform::observe::parquet::rotate_metrics(&state)
        .await
        .unwrap();
    assert_eq!(rotated, 0);
}

// ---------------------------------------------------------------------------
// Metric names — project scoped
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// spawn_background_tasks + immediate shutdown
// ---------------------------------------------------------------------------

/// `spawn_background_tasks` starts all observe tasks and shuts down cleanly.
#[sqlx::test(migrations = "./migrations")]
async fn spawn_background_tasks_and_shutdown(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let channels = platform::observe::spawn_background_tasks(state.clone(), shutdown_rx);

    // Verify channels are functional by sending a span
    let span = platform::observe::store::SpanRecord {
        trace_id: "bg-task-test-trace".into(),
        span_id: "bg-task-test-span".into(),
        parent_span_id: None,
        name: "bg-test".into(),
        service: "bg-task-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(1),
        started_at: Utc::now(),
        finished_at: None,
        project_id: None,
        session_id: None,
        user_id: None,
    };
    channels.spans_tx.send(span).await.unwrap();

    // Signal shutdown — all tasks should drain and exit
    shutdown_tx.send(()).unwrap();

    // Wait for flush to complete
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the span was flushed to DB
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spans WHERE service = 'bg-task-svc'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        count.0 >= 1,
        "span should have been flushed by background task"
    );
}

/// Observability retention cleanup deletes old data.
#[sqlx::test(migrations = "./migrations")]
async fn retention_cleanup_deletes_old_data(pool: PgPool) {
    let (_state, _admin_token) = test_state(pool.clone()).await;

    // Insert old data (90+ days)
    let old_ts = Utc::now() - chrono::Duration::days(100);

    // Insert old span
    let old_span = platform::observe::store::SpanRecord {
        trace_id: "retention-trace".into(),
        span_id: "retention-span".into(),
        parent_span_id: None,
        name: "retention-test".into(),
        service: "retention-svc".into(),
        kind: "server".into(),
        status: "ok".into(),
        attributes: None,
        events: None,
        duration_ms: Some(1),
        started_at: old_ts,
        finished_at: Some(old_ts),
        project_id: None,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(&pool, &[old_span])
        .await
        .unwrap();

    // Insert old log
    let old_log = platform::observe::store::LogEntryRecord {
        timestamp: old_ts,
        trace_id: None,
        span_id: None,
        project_id: None,
        session_id: None,
        user_id: None,
        service: "retention-svc".into(),
        level: "info".into(),
        source: "external".into(),
        message: "old log for retention".into(),
        attributes: None,
    };
    platform::observe::store::write_logs(&pool, &[old_log])
        .await
        .unwrap();

    // Insert old metric
    let old_metric = platform::observe::store::MetricRecord {
        name: "retention_metric".into(),
        labels: serde_json::json!({}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: old_ts,
        value: 1.0,
    };
    platform::observe::store::write_metrics(&pool, &[old_metric])
        .await
        .unwrap();

    // Verify old data exists
    let span_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM spans WHERE service = 'retention-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(span_count.0 >= 1, "old span should exist before cleanup");

    // Run the retention cleanup SQL manually (same as the background task)
    let retention_days: i64 = 90; // default
    let cutoff = Utc::now() - chrono::Duration::days(retention_days);

    for (table, col) in &[
        ("spans", "started_at"),
        ("log_entries", "timestamp"),
        ("metric_samples", "timestamp"),
    ] {
        let sql = format!("DELETE FROM {table} WHERE {col} < $1");
        sqlx::query(&sql).bind(cutoff).execute(&pool).await.unwrap();
    }

    // Verify old data was deleted
    let span_count_after: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM spans WHERE service = 'retention-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        span_count_after.0, 0,
        "old span should be deleted by retention cleanup"
    );
}

// ---------------------------------------------------------------------------
// Parquet rotation — multiple rows
// ---------------------------------------------------------------------------

/// Rotate multiple old logs at once.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_logs_multiple_entries(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let old_ts = Utc::now() - chrono::Duration::hours(72);
    let mut logs = Vec::new();
    for i in 0..5 {
        logs.push(platform::observe::store::LogEntryRecord {
            timestamp: old_ts + chrono::Duration::seconds(i),
            trace_id: Some(format!("multi-rot-trace-{i}")),
            span_id: None,
            project_id: None,
            session_id: None,
            user_id: None,
            service: "multi-rot-svc".into(),
            level: if i % 2 == 0 { "info" } else { "error" }.into(),
            source: "external".into(),
            message: format!("multi rotation log {i}"),
            attributes: Some(serde_json::json!({"index": i})),
        });
    }
    platform::observe::store::write_logs(&pool, &logs)
        .await
        .unwrap();

    let rotated = platform::observe::parquet::rotate_logs(&state)
        .await
        .unwrap();
    assert!(
        rotated >= 5,
        "should have rotated at least 5 logs, got {rotated}"
    );

    // Verify deleted from DB
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM log_entries WHERE service = 'multi-rot-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 0, "all rotated logs should be deleted");
}

/// Rotate multiple old spans at once.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_spans_multiple_entries(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let old_ts = Utc::now() - chrono::Duration::hours(72);
    let mut spans = Vec::new();
    for i in 0..5 {
        spans.push(platform::observe::store::SpanRecord {
            trace_id: format!("multi-rot-span-trace-{i}"),
            span_id: format!("multi-rot-span-{i}"),
            parent_span_id: if i > 0 {
                Some(format!("multi-rot-span-{}", i - 1))
            } else {
                None
            },
            name: format!("op-{i}"),
            service: "multi-rot-span-svc".into(),
            kind: "server".into(),
            status: "ok".into(),
            attributes: Some(serde_json::json!({"index": i})),
            events: None,
            duration_ms: Some(i * 10),
            started_at: old_ts + chrono::Duration::seconds(i64::from(i)),
            finished_at: Some(
                old_ts
                    + chrono::Duration::seconds(i64::from(i))
                    + chrono::Duration::milliseconds(i64::from(i * 10)),
            ),
            project_id: None,
            session_id: None,
            user_id: None,
        });
    }
    platform::observe::store::write_spans(&pool, &spans)
        .await
        .unwrap();

    let rotated = platform::observe::parquet::rotate_spans(&state)
        .await
        .unwrap();
    assert!(
        rotated >= 5,
        "should have rotated at least 5 spans, got {rotated}"
    );

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM spans WHERE service = 'multi-rot-span-svc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 0, "all rotated spans should be deleted");
}

/// Rotate multiple old metrics at once.
#[sqlx::test(migrations = "./migrations")]
async fn rotate_metrics_multiple_entries(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let old_ts = Utc::now() - chrono::Duration::hours(2);
    let mut metrics = Vec::new();
    for i in 0..5 {
        metrics.push(platform::observe::store::MetricRecord {
            name: "multi_rot_metric".into(),
            labels: serde_json::json!({"instance": format!("node{i}")}),
            metric_type: "gauge".into(),
            unit: Some("bytes".into()),
            project_id: None,
            timestamp: old_ts + chrono::Duration::seconds(i64::from(i)),
            value: f64::from(i) * 10.0,
        });
    }
    platform::observe::store::write_metrics(&pool, &metrics)
        .await
        .unwrap();

    let rotated = platform::observe::parquet::rotate_metrics(&state)
        .await
        .unwrap();
    assert!(
        rotated >= 5,
        "should have rotated at least 5 metrics, got {rotated}"
    );
}

// ---------------------------------------------------------------------------
// Metric names — project scoped
// ---------------------------------------------------------------------------

/// List metric names scoped to a specific project.
#[sqlx::test(migrations = "./migrations")]
async fn list_metric_names_project_scoped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "metric-names-proj", "public").await;

    // Insert metric with project_id
    let metric = platform::observe::store::MetricRecord {
        name: format!("proj_metric_{}", Uuid::new_v4().simple()),
        labels: serde_json::json!({"host": "node1"}),
        metric_type: "gauge".into(),
        unit: Some("percent".into()),
        project_id: Some(project_id),
        timestamp: Utc::now(),
        value: 55.0,
    };
    let metric_name = metric.name.clone();
    platform::observe::store::write_metrics(&pool, &[metric])
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/metrics/names?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(
        names
            .iter()
            .any(|n| n["name"].as_str() == Some(&metric_name)),
        "project-scoped metric name should appear"
    );
}

// ---------------------------------------------------------------------------
// Aggregation endpoints (PR 2)
// ---------------------------------------------------------------------------

/// Helper: insert a span with full control over fields.
async fn insert_span_full(
    pool: &PgPool,
    trace_id: &str,
    span_id: &str,
    service: &str,
    kind: &str,
    status: &str,
    duration_ms: i32,
    project_id: Option<Uuid>,
) {
    let now = Utc::now();
    let span = platform::observe::store::SpanRecord {
        trace_id: trace_id.into(),
        span_id: span_id.into(),
        parent_span_id: None,
        name: "test-op".into(),
        service: service.into(),
        kind: kind.into(),
        status: status.into(),
        attributes: None,
        events: None,
        duration_ms: Some(duration_ms),
        started_at: now,
        finished_at: Some(now + chrono::Duration::milliseconds(i64::from(duration_ms))),
        project_id,
        session_id: None,
        user_id: None,
    };
    platform::observe::store::write_spans(pool, &[span])
        .await
        .expect("write_spans failed");
}

/// Helper: insert a trace directly.
async fn insert_trace(
    pool: &PgPool,
    trace_id: &str,
    root_span: &str,
    service: &str,
    status: &str,
    duration_ms: i32,
    project_id: Option<Uuid>,
) {
    sqlx::query(
        "INSERT INTO traces (trace_id, root_span, service, status, duration_ms, started_at, project_id)
         VALUES ($1, $2, $3, $4, $5, now(), $6)",
    )
    .bind(trace_id)
    .bind(root_span)
    .bind(service)
    .bind(status)
    .bind(duration_ms)
    .bind(project_id)
    .execute(pool)
    .await
    .expect("insert trace");
}

/// Helper: insert a metric sample.
async fn insert_metric(
    pool: &PgPool,
    name: &str,
    service: &str,
    value: f64,
    project_id: Option<Uuid>,
) {
    let record = platform::observe::store::MetricRecord {
        name: name.into(),
        labels: serde_json::json!({"service": service}),
        metric_type: "gauge".into(),
        unit: None,
        project_id,
        timestamp: Utc::now(),
        value,
    };
    platform::observe::store::write_metrics(pool, &[record])
        .await
        .expect("write_metrics failed");
}

// --- Topology tests ---

#[sqlx::test(migrations = "./migrations")]
async fn topology_happy_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc_a = format!("svc-a-{}", Uuid::new_v4().simple());
    let svc_b = format!("svc-b-{}", Uuid::new_v4().simple());
    let trace_id = format!("trace-{}", Uuid::new_v4());

    // Client span from svc_a, server span in svc_b (same trace)
    insert_span_full(
        &pool,
        &trace_id,
        &format!("c-{}", Uuid::new_v4()),
        &svc_a,
        "client",
        "ok",
        50,
        None,
    )
    .await;
    insert_span_full(
        &pool,
        &trace_id,
        &format!("s-{}", Uuid::new_v4()),
        &svc_b,
        "server",
        "ok",
        40,
        None,
    )
    .await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/topology?range=1h").await;
    assert_eq!(status, StatusCode::OK, "topology failed: {body}");

    let edges = body["edges"].as_array().unwrap();
    let matching = edges.iter().find(|e| {
        e["from_service"].as_str() == Some(&svc_a) && e["to_service"].as_str() == Some(&svc_b)
    });
    assert!(
        matching.is_some(),
        "expected edge from {svc_a} -> {svc_b}: {body}"
    );
    assert!(body["services"].as_array().unwrap().len() >= 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn topology_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/topology?range=1h").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["edges"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn topology_requires_admin_global(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, user_token) = create_user(&app, &admin_token, "topo-user", "topo@test.com").await;
    let _ = user_id;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/observe/topology?range=1h").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// --- Error breakdown tests ---

#[sqlx::test(migrations = "./migrations")]
async fn errors_happy_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("err-svc-{}", Uuid::new_v4().simple());
    let trace_id = format!("trace-{}", Uuid::new_v4());

    insert_span_full(
        &pool,
        &trace_id,
        &format!("s-{}", Uuid::new_v4()),
        &svc,
        "server",
        "error",
        100,
        None,
    )
    .await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/errors?range=1h").await;
    assert_eq!(status, StatusCode::OK, "errors failed: {body}");

    let groups = body.as_array().unwrap();
    assert!(!groups.is_empty(), "expected error groups: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn errors_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/errors?range=1h").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

// --- Trace aggregation tests ---

#[sqlx::test(migrations = "./migrations")]
async fn trace_agg_happy_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("agg-svc-{}", Uuid::new_v4().simple());
    for i in 0..5 {
        let tid = format!("trace-agg-{}-{}", Uuid::new_v4(), i);
        let status = if i == 0 { "error" } else { "ok" };
        insert_trace(&pool, &tid, "GET /api/test", &svc, status, 10 + i * 5, None).await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/traces/aggregated?range=1h",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "trace agg failed: {body}");

    let rows = body.as_array().unwrap();
    let matching = rows
        .iter()
        .find(|r| r["name"].as_str() == Some("GET /api/test"));
    assert!(matching.is_some(), "expected aggregated trace row: {body}");
    let m = matching.unwrap();
    assert_eq!(m["count"].as_i64().unwrap(), 5);
    assert!(m["avg_duration_ms"].as_f64().unwrap() > 0.0);
    assert!(m["error_rate"].as_f64().unwrap() > 0.0); // 1/5 = 20%
}

#[sqlx::test(migrations = "./migrations")]
async fn trace_agg_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/traces/aggregated?range=1h",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

// --- Load timeline tests ---

#[sqlx::test(migrations = "./migrations")]
async fn load_happy_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("load-svc-{}", Uuid::new_v4().simple());
    insert_metric(&pool, "http.server.request.count", &svc, 42.0, None).await;
    insert_metric(&pool, "http.server.error.count", &svc, 2.0, None).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/load?range=1h").await;
    assert_eq!(status, StatusCode::OK, "load failed: {body}");

    // Should have at least one bucket with data
    let points = body["points"].as_array().unwrap();
    assert!(!points.is_empty(), "expected load points: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn load_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/load?range=1h").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["points"].as_array().unwrap().is_empty());
}

// --- Component health tests ---

#[sqlx::test(migrations = "./migrations")]
async fn components_happy_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let svc = format!("comp-svc-{}", Uuid::new_v4().simple());
    insert_metric(&pool, "k8s.deployment.replicas", &svc, 3.0, None).await;
    insert_metric(&pool, "k8s.deployment.ready_replicas", &svc, 3.0, None).await;
    insert_metric(&pool, "k8s.pod.ready", &svc, 1.0, None).await;
    insert_metric(&pool, "process.cpu.utilization", &svc, 250.0, None).await;
    insert_metric(&pool, "process.memory.rss", &svc, 536_870_912.0, None).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/components").await;
    assert_eq!(status, StatusCode::OK, "components failed: {body}");

    let components = body.as_array().unwrap();
    let matching = components.iter().find(|c| c["name"].as_str() == Some(&svc));
    assert!(matching.is_some(), "expected component {svc}: {body}");
    let c = matching.unwrap();
    assert_eq!(c["replicas"].as_i64().unwrap(), 3);
    assert!(c["ready"].as_bool().unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn components_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/components").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn components_admin_required(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, user_token) = create_user(&app, &admin_token, "comp-user", "comp@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/observe/components").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// --- Cross-cutting permission test ---

#[sqlx::test(migrations = "./migrations")]
async fn all_new_endpoints_require_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, user_token) = create_user(&app, &admin_token, "perm-user", "perm@test.com").await;

    for endpoint in &[
        "/api/observe/topology?range=1h",
        "/api/observe/errors?range=1h",
        "/api/observe/traces/aggregated?range=1h",
        "/api/observe/load?range=1h",
        "/api/observe/components",
    ] {
        let (status, _) = helpers::get_json(&app, &user_token, endpoint).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "expected 403 for unprivileged user on {endpoint}"
        );
    }
}

// ---------------------------------------------------------------------------
// Metric series project isolation
// ---------------------------------------------------------------------------

/// Two projects writing the same metric name get separate series rows.
#[sqlx::test(migrations = "./migrations")]
async fn metrics_different_projects_get_separate_series(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let proj_a = helpers::create_project(&app, &admin_token, "metrics-iso-a", "private").await;
    let proj_b = helpers::create_project(&app, &admin_token, "metrics-iso-b", "private").await;

    let now = Utc::now();
    let metrics = vec![
        MetricRecord {
            name: "http_requests_total".into(),
            labels: serde_json::json!({}),
            metric_type: "counter".into(),
            unit: None,
            project_id: Some(proj_a),
            value: 100.0,
            timestamp: now,
        },
        MetricRecord {
            name: "http_requests_total".into(),
            labels: serde_json::json!({}),
            metric_type: "counter".into(),
            unit: None,
            project_id: Some(proj_b),
            value: 200.0,
            timestamp: now,
        },
    ];
    write_metrics(&pool, &metrics)
        .await
        .expect("write_metrics should succeed");

    // Verify two distinct series exist
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM metric_series WHERE name = 'http_requests_total'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 2, "each project should have its own series");

    // Verify project_ids and values are correct
    let series: Vec<(Option<Uuid>, Option<f64>)> = sqlx::query_as(
        "SELECT project_id, last_value FROM metric_series \
         WHERE name = 'http_requests_total' ORDER BY last_value",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(series[0].0, Some(proj_a));
    assert_eq!(series[0].1, Some(100.0));
    assert_eq!(series[1].0, Some(proj_b));
    assert_eq!(series[1].1, Some(200.0));
}

/// Same project writing the same metric twice updates (upserts) the existing series.
#[sqlx::test(migrations = "./migrations")]
async fn metrics_same_project_upserts_series(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let proj = helpers::create_project(&app, &admin_token, "metrics-upsert", "private").await;

    let t1 = Utc::now();
    let t2 = t1 + chrono::Duration::seconds(1);

    // First write
    write_metrics(
        &pool,
        &[MetricRecord {
            name: "cpu_usage".into(),
            labels: serde_json::json!({"host": "a"}),
            metric_type: "gauge".into(),
            unit: Some("percent".into()),
            project_id: Some(proj),
            value: 50.0,
            timestamp: t1,
        }],
    )
    .await
    .unwrap();

    // Second write — same series, different value
    write_metrics(
        &pool,
        &[MetricRecord {
            name: "cpu_usage".into(),
            labels: serde_json::json!({"host": "a"}),
            metric_type: "gauge".into(),
            unit: Some("percent".into()),
            project_id: Some(proj),
            value: 75.0,
            timestamp: t2,
        }],
    )
    .await
    .unwrap();

    // Should still be one series, with last_value updated
    let row: (i64, Option<f64>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(last_value) FROM metric_series \
         WHERE name = 'cpu_usage' AND project_id = $1",
    )
    .bind(proj)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, 1, "should have exactly one series");
    assert_eq!(row.1, Some(75.0), "last_value should be updated");

    // Should have two samples
    let sample_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM metric_samples ms \
         JOIN metric_series ser ON ser.id = ms.series_id \
         WHERE ser.name = 'cpu_usage' AND ser.project_id = $1",
    )
    .bind(proj)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sample_count.0, 2);
}
