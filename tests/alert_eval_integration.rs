//! Integration tests for alert evaluation — evaluate_metric, fire_alert, resolve_alert, evaluate_all.

mod helpers;

use axum::http::StatusCode;
use chrono::Utc;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use helpers::{admin_login, test_router, test_state};

/// Insert metric data directly via store layer.
async fn insert_metric(pool: &PgPool, name: &str, value: f64) {
    let metric = platform::observe::store::MetricRecord {
        name: name.into(),
        labels: serde_json::json!({"host": "eval-test"}),
        metric_type: "gauge".into(),
        unit: None,
        project_id: None,
        timestamp: Utc::now(),
        value,
    };
    platform::observe::store::write_metrics(pool, &[metric])
        .await
        .expect("write_metrics failed");
}

/// Insert an alert rule directly in DB (bypasses API validation for simplicity).
async fn insert_alert_rule(
    pool: &PgPool,
    name: &str,
    query: &str,
    condition: &str,
    threshold: Option<f64>,
    for_seconds: i32,
) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        r"INSERT INTO alert_rules (name, query, condition, threshold, for_seconds, severity, enabled)
          VALUES ($1, $2, $3, $4, $5, 'warning', true)
          RETURNING id",
    )
    .bind(name)
    .bind(query)
    .bind(condition)
    .bind(threshold)
    .bind(for_seconds)
    .fetch_one(pool)
    .await
    .expect("insert alert rule");
    row.0
}

/// Count alert events for a rule.
async fn count_alert_events(pool: &PgPool, rule_id: Uuid) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert_events WHERE rule_id = $1")
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Get latest alert event status.
async fn latest_event_status(pool: &PgPool, rule_id: Uuid) -> Option<String> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT status FROM alert_events WHERE rule_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(rule_id)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(|r| r.0)
}

// ---------------------------------------------------------------------------
// evaluate_metric aggregation tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_avg(pool: PgPool) {
    insert_metric(&pool, "eval_avg_test", 10.0).await;
    insert_metric(&pool, "eval_avg_test", 20.0).await;
    insert_metric(&pool, "eval_avg_test", 30.0).await;

    let result =
        platform::observe::alert::evaluate_metric(&pool, "eval_avg_test", None, "avg", 300)
            .await
            .unwrap();
    let v = result.unwrap();
    assert!((v - 20.0).abs() < 0.01, "avg should be 20.0, got {v}");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_sum(pool: PgPool) {
    insert_metric(&pool, "eval_sum_test", 10.0).await;
    insert_metric(&pool, "eval_sum_test", 20.0).await;

    let result =
        platform::observe::alert::evaluate_metric(&pool, "eval_sum_test", None, "sum", 300)
            .await
            .unwrap();
    let v = result.unwrap();
    assert!((v - 30.0).abs() < 0.01, "sum should be 30.0, got {v}");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_max(pool: PgPool) {
    insert_metric(&pool, "eval_max_test", 5.0).await;
    insert_metric(&pool, "eval_max_test", 99.0).await;
    insert_metric(&pool, "eval_max_test", 42.0).await;

    let result =
        platform::observe::alert::evaluate_metric(&pool, "eval_max_test", None, "max", 300)
            .await
            .unwrap();
    let v = result.unwrap();
    assert!((v - 99.0).abs() < 0.01, "max should be 99.0, got {v}");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_min(pool: PgPool) {
    insert_metric(&pool, "eval_min_test", 5.0).await;
    insert_metric(&pool, "eval_min_test", 99.0).await;
    insert_metric(&pool, "eval_min_test", 42.0).await;

    let result =
        platform::observe::alert::evaluate_metric(&pool, "eval_min_test", None, "min", 300)
            .await
            .unwrap();
    let v = result.unwrap();
    assert!((v - 5.0).abs() < 0.01, "min should be 5.0, got {v}");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_count(pool: PgPool) {
    insert_metric(&pool, "eval_count_test", 1.0).await;
    insert_metric(&pool, "eval_count_test", 2.0).await;
    insert_metric(&pool, "eval_count_test", 3.0).await;
    insert_metric(&pool, "eval_count_test", 4.0).await;

    let result =
        platform::observe::alert::evaluate_metric(&pool, "eval_count_test", None, "count", 300)
            .await
            .unwrap();
    let v = result.unwrap();
    assert!((v - 4.0).abs() < 0.01, "count should be 4.0, got {v}");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_no_data_returns_none(pool: PgPool) {
    let result = platform::observe::alert::evaluate_metric(
        &pool,
        "nonexistent_metric_eval",
        None,
        "avg",
        300,
    )
    .await
    .unwrap();
    assert!(result.is_none(), "missing metric should return None");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_metric_unknown_agg_returns_none(pool: PgPool) {
    insert_metric(&pool, "eval_unknown_agg", 10.0).await;

    let result =
        platform::observe::alert::evaluate_metric(&pool, "eval_unknown_agg", None, "median", 300)
            .await
            .unwrap();
    assert!(result.is_none(), "unknown agg should return None");
}

// ---------------------------------------------------------------------------
// fire_alert / resolve_alert
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn fire_and_resolve_alert(pool: PgPool) {
    let rule_id = insert_alert_rule(
        &pool,
        "fire-test",
        "metric:cpu agg:avg window:60",
        "gt",
        Some(50.0),
        10,
    )
    .await;

    // Fire
    platform::observe::alert::fire_alert(&pool, rule_id, Some(75.0))
        .await
        .unwrap();

    assert_eq!(count_alert_events(&pool, rule_id).await, 1);
    assert_eq!(
        latest_event_status(&pool, rule_id).await.as_deref(),
        Some("firing")
    );

    // Resolve
    platform::observe::alert::resolve_alert(&pool, rule_id)
        .await
        .unwrap();

    assert_eq!(
        latest_event_status(&pool, rule_id).await.as_deref(),
        Some("resolved")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn resolve_alert_without_firing_is_noop(pool: PgPool) {
    let rule_id = insert_alert_rule(
        &pool,
        "noop-resolve",
        "metric:cpu agg:avg window:60",
        "gt",
        Some(50.0),
        10,
    )
    .await;

    // Resolve with no firing event — should not error
    platform::observe::alert::resolve_alert(&pool, rule_id)
        .await
        .unwrap();

    assert_eq!(count_alert_events(&pool, rule_id).await, 0);
}

// ---------------------------------------------------------------------------
// evaluate_all (full cycle)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_all_fires_when_threshold_exceeded(pool: PgPool) {
    let state = test_state(pool.clone()).await;

    // Create alert rule: avg(cpu_eval_test) > 80.0 for 0 seconds (immediate)
    let rule_id = insert_alert_rule(
        &pool,
        "eval-all-test",
        "metric:cpu_eval_test agg:avg window:300",
        "gt",
        Some(80.0),
        10, // minimum for_seconds
    )
    .await;

    // Insert metric data that exceeds threshold
    insert_metric(&pool, "cpu_eval_test", 95.0).await;

    // First evaluation — sets first_triggered but doesn't fire (for_seconds not met)
    let mut alert_states = HashMap::new();
    platform::observe::alert::evaluate_all(&state, &mut alert_states)
        .await
        .unwrap();

    // Pretend the condition has been true for longer than for_seconds by
    // backdating the first_triggered time in the state map
    if let Some(s) = alert_states.get_mut(&rule_id) {
        s.first_triggered = Some(Utc::now() - chrono::Duration::seconds(60));
    }

    // Second evaluation — should fire since condition held longer than for_seconds
    platform::observe::alert::evaluate_all(&state, &mut alert_states)
        .await
        .unwrap();

    assert_eq!(count_alert_events(&pool, rule_id).await, 1);
    assert_eq!(
        latest_event_status(&pool, rule_id).await.as_deref(),
        Some("firing")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_all_resolves_when_below_threshold(pool: PgPool) {
    let state = test_state(pool.clone()).await;

    let rule_id = insert_alert_rule(
        &pool,
        "eval-resolve-test",
        "metric:cpu_resolve_test agg:avg window:300",
        "gt",
        Some(80.0),
        10,
    )
    .await;

    // Simulate a firing state
    platform::observe::alert::fire_alert(&pool, rule_id, Some(95.0))
        .await
        .unwrap();

    // Insert metric data BELOW threshold
    insert_metric(&pool, "cpu_resolve_test", 50.0).await;

    // Seed alert states with the rule in firing state
    let mut alert_states = HashMap::new();
    alert_states.insert(
        rule_id,
        platform::observe::alert::AlertState {
            first_triggered: Some(Utc::now() - chrono::Duration::seconds(60)),
            firing: true,
        },
    );

    // Run evaluate_all — metric is 50 (below 80), condition is false,
    // and firing=true, so it should resolve.
    platform::observe::alert::evaluate_all(&state, &mut alert_states)
        .await
        .unwrap();

    // Check that the alert was resolved
    assert_eq!(
        latest_event_status(&pool, rule_id).await.as_deref(),
        Some("resolved")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_all_skips_disabled_rules(pool: PgPool) {
    let state = test_state(pool.clone()).await;

    // Create a disabled alert rule
    sqlx::query(
        "INSERT INTO alert_rules (name, query, condition, threshold, for_seconds, severity, enabled)
         VALUES ('disabled-rule', 'metric:should_skip agg:avg window:60', 'gt', 50.0, 10, 'warning', false)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Insert metric data
    insert_metric(&pool, "should_skip", 99.0).await;

    // Run evaluation
    let mut alert_states = HashMap::new();
    platform::observe::alert::evaluate_all(&state, &mut alert_states)
        .await
        .unwrap();

    // No events should have been created for any rule
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0, "disabled rule should not generate events");
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_all_absent_condition(pool: PgPool) {
    let state = test_state(pool.clone()).await;

    let rule_id = insert_alert_rule(
        &pool,
        "absent-test",
        "metric:nonexistent_metric_xxx agg:avg window:300",
        "absent",
        None,
        10,
    )
    .await;

    // First eval — sets first_triggered
    let mut alert_states = HashMap::new();
    platform::observe::alert::evaluate_all(&state, &mut alert_states)
        .await
        .unwrap();

    // Backdate first_triggered
    if let Some(s) = alert_states.get_mut(&rule_id) {
        s.first_triggered = Some(Utc::now() - chrono::Duration::seconds(60));
    }

    // Second eval — should fire (metric is absent)
    platform::observe::alert::evaluate_all(&state, &mut alert_states)
        .await
        .unwrap();

    assert_eq!(count_alert_events(&pool, rule_id).await, 1);
    assert_eq!(
        latest_event_status(&pool, rule_id).await.as_deref(),
        Some("firing")
    );
}

// ---------------------------------------------------------------------------
// Alert API — list filtering
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_alerts_filter_by_enabled(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Create two alerts
    helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "enabled-alert",
            "query": "metric:cpu agg:avg window:60",
            "condition": "gt",
            "threshold": 50.0,
        }),
    )
    .await;

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "to-disable",
            "query": "metric:mem agg:avg window:60",
            "condition": "gt",
            "threshold": 90.0,
        }),
    )
    .await;
    let alert_id = body["id"].as_str().unwrap();

    // Disable one
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
        serde_json::json!({"enabled": false}),
    )
    .await;

    // Filter by enabled=true
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/alerts?enabled=true").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 1);
    assert_eq!(body["items"][0]["name"], "enabled-alert");

    // Filter by enabled=false
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/observe/alerts?enabled=false").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 1);
    assert_eq!(body["items"][0]["name"], "to-disable");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_alert_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_alert_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_alert_invalid_severity(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "bad-severity",
            "query": "metric:cpu agg:avg window:60",
            "condition": "gt",
            "threshold": 50.0,
            "severity": "catastrophic",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_alert_invalid_condition(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "bad-condition",
            "query": "metric:cpu agg:avg window:60",
            "condition": "gte",
            "threshold": 50.0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_alert_for_seconds_out_of_range(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "bad-for-seconds",
            "query": "metric:cpu agg:avg window:60",
            "condition": "gt",
            "threshold": 50.0,
            "for_seconds": 5,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_alert_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{fake_id}"),
        serde_json::json!({"name": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn alert_events_with_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let rule_id = insert_alert_rule(
        &pool,
        "events-data",
        "metric:cpu agg:avg window:60",
        "gt",
        Some(50.0),
        10,
    )
    .await;

    // Insert a firing event
    platform::observe::alert::fire_alert(&pool, rule_id, Some(75.0))
        .await
        .unwrap();

    // Query events via API
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{rule_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 1);
    assert_eq!(body["items"][0]["status"], "firing");
    assert!((body["items"][0]["value"].as_f64().unwrap() - 75.0).abs() < 0.01);

    // Also check all-events endpoint
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/alerts/events").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["total"].as_i64().unwrap() >= 1);
}
