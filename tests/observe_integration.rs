//! Integration tests for the observability module (traces, logs, metrics, alerts).

mod helpers;

use axum::http::StatusCode;
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{admin_login, create_user, test_router, test_state};

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
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

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
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

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
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

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
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

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
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

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
    assert_eq!(status, StatusCode::OK);

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
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Create user with NO role at all — no observe:read
    let (_user_id, token) = create_user(&app, &admin_token, "no-observe", "noobs@test.com").await;

    let (status, _) = helpers::get_json(&app, &token, "/api/observe/traces").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = helpers::get_json(&app, &token, "/api/observe/logs").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
