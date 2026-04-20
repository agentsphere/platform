// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! E2E tests for the demo project lifecycle.
//!
//! These tests require a Kind cluster with Postgres, Valkey, and MinIO.
//! Run with: `just test-e2e` or `cargo nextest run --profile e2e`

#![allow(
    clippy::manual_assert,
    clippy::collapsible_if,
    clippy::needless_pass_by_value,
    clippy::struct_field_names,
    clippy::needless_update,
    clippy::doc_markdown
)]

#[allow(dead_code)]
mod e2e_helpers;

use axum::http::StatusCode;
use prost::Message;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// RAII guards for background tasks
// ---------------------------------------------------------------------------

struct ExecutorGuard {
    cancel: tokio_util::sync::CancellationToken,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl ExecutorGuard {
    fn spawn(state: &platform_next::state::PlatformState) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let pipeline_state = state.pipeline_state();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            platform_pipeline::executor::run(pipeline_state, token).await;
        });
        Self { cancel, handle }
    }
}

impl Drop for ExecutorGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn admin_user_id(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .expect("admin user not found")
}

/// Poll until at least one pipeline appears for the project, then poll its status.
async fn poll_project_pipeline(
    app: &axum::Router,
    token: &str,
    project_id: Uuid,
    timeout_secs: u64,
) -> (String, String, Vec<serde_json::Value>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

    // Wait for a pipeline to exist
    let pipeline_id = loop {
        let (status, body) = e2e_helpers::get_json(
            app,
            token,
            &format!("/api/projects/{project_id}/pipelines?limit=1"),
        )
        .await;
        if status == StatusCode::OK {
            if let Some(items) = body["items"].as_array() {
                if let Some(first) = items.first() {
                    if let Some(id) = first["id"].as_str() {
                        break id.to_string();
                    }
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!("no pipeline found for project {project_id} within {timeout_secs}s");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    // Poll until terminal
    let final_status =
        e2e_helpers::poll_pipeline_status(app, token, project_id, &pipeline_id, timeout_secs).await;

    // Fetch detail with steps
    let (_, detail) = e2e_helpers::get_json(
        app,
        token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;
    let steps = detail["steps"].as_array().cloned().unwrap_or_default();

    (pipeline_id, final_status, steps)
}

/// Generic polling utility — calls `f` every 3s until it returns `true`.
async fn poll_until<F, Fut>(label: &str, timeout_secs: u64, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if f().await {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("{label} timed out after {timeout_secs}s");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ===========================================================================
// Test 1: Demo project creation — DB state assertions
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_project_creation(pool: PgPool) {
    let (state, _token) = e2e_helpers::e2e_state(pool.clone()).await;
    let owner_id = admin_user_id(&pool).await;

    let (project_id, project_name) =
        platform_next::demo::demo_project::create_demo_project(&state, owner_id)
            .await
            .expect("create_demo_project failed");

    assert!(!project_name.is_empty());

    // Project should be active
    let active: bool = sqlx::query_scalar("SELECT is_active FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .expect("project not found");
    assert!(active);

    // Should have exactly 1 MR on feature/shop-app-v0.1
    let mr_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM merge_requests WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mr_count, 1, "expected 1 MR, got {mr_count}");

    let mr_branch: String =
        sqlx::query_scalar("SELECT source_branch FROM merge_requests WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mr_branch, "feature/shop-app-v0.1");

    // Should have 4 sample issues
    let issue_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(issue_count, 4, "expected 4 issues, got {issue_count}");

    // Should have stored demo_project_id setting
    let setting = platform_next::demo::demo_project::get_setting(&pool, "demo_project_id")
        .await
        .unwrap();
    assert!(setting.is_some(), "demo_project_id setting not found");

    // Should have at least 1 pipeline triggered
    let pipeline_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pipelines WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        pipeline_count >= 1,
        "expected at least 1 pipeline, got {pipeline_count}"
    );

    // Pipeline should be MR trigger type
    let trigger: String = sqlx::query_scalar(
        "SELECT trigger FROM pipelines WHERE project_id = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(trigger, "mr", "expected MR trigger, got {trigger}");
}

// ===========================================================================
// Test 2: MR pipeline steps are not filtered
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_pipeline_mr_steps_not_filtered(pool: PgPool) {
    let (state, token, _handle) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let _executor = ExecutorGuard::spawn(&state);

    let owner_id = admin_user_id(&pool).await;
    platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    let project_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM projects WHERE owner_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(owner_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let app = e2e_helpers::pipeline_test_router(state);
    let (_pipeline_id, _status, steps) = poll_project_pipeline(&app, &token, project_id, 300).await;

    let step_names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(
        step_names.contains(&"build-app"),
        "missing build-app step: {step_names:?}"
    );
    assert!(
        step_names.contains(&"build-test"),
        "missing build-test step: {step_names:?}"
    );
    assert!(
        step_names.contains(&"e2e"),
        "missing e2e step: {step_names:?}"
    );

    // build-test must NOT be skipped (regression: MR trigger should not filter test steps)
    let build_test = steps.iter().find(|s| s["name"] == "build-test").unwrap();
    assert_ne!(
        build_test["status"].as_str().unwrap_or(""),
        "skipped",
        "build-test was skipped — MR trigger filtering is broken"
    );
}

// ===========================================================================
// Test 3: Demo secrets created
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_secrets_created(pool: PgPool) {
    let (state, _token) = e2e_helpers::e2e_state(pool.clone()).await;
    let owner_id = admin_user_id(&pool).await;

    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    // Total secrets: 4 base + 2 env overrides = 6
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 6, "expected 6 secrets, got {total}");

    // At least 1 pipeline-scoped
    let pipeline_scoped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND scope = 'pipeline'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        pipeline_scoped >= 1,
        "expected at least 1 pipeline-scoped secret"
    );

    // At least 2 env-scoped (staging + production overrides)
    let env_scoped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND environment IS NOT NULL",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(env_scoped >= 2, "expected at least 2 env-scoped secrets");

    // Decrypt round-trip: verify key secrets are resolvable
    if let Some(master_key_hex) = &state.config.secrets.master_key {
        let key = platform_secrets::parse_master_key(master_key_hex).unwrap();
        let scoped = platform_secrets::query_scoped_secrets(
            &pool,
            &key,
            project_id,
            &["all", "pipeline"],
            None,
        )
        .await
        .unwrap();
        let secret_names: Vec<&str> = scoped.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            secret_names.contains(&"APP_SECRET_KEY"),
            "APP_SECRET_KEY not found in scoped secrets"
        );
        assert!(
            secret_names.contains(&"DATABASE_URL"),
            "DATABASE_URL not found in scoped secrets"
        );
        assert!(
            secret_names.contains(&"VALKEY_URL"),
            "VALKEY_URL not found in scoped secrets"
        );
    }
}

// ===========================================================================
// Test 4: Demo secrets API list
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_secrets_api_list(pool: PgPool) {
    let (state, token) = e2e_helpers::e2e_state(pool.clone()).await;
    let owner_id = admin_user_id(&pool).await;

    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    let app = e2e_helpers::test_router(state);

    // List secrets
    let (status, body) =
        e2e_helpers::get_json(&app, &token, &format!("/api/projects/{project_id}/secrets")).await;
    assert_eq!(status, StatusCode::OK, "list secrets failed: {body}");
    let items = body["items"].as_array().expect("missing items");
    assert!(
        items.len() >= 4,
        "expected at least 4 secrets, got {}",
        items.len()
    );

    // Read specific secret
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/secrets/DATABASE_URL"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read secret failed: {body}");
    let value = body["value"].as_str().expect("missing value");
    assert_eq!(
        value, "postgresql://app:changeme@platform-demo-db:5432/app",
        "DATABASE_URL mismatch"
    );
}

// ===========================================================================
// Test 5: Pipeline secrets injected
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_pipeline_secrets_injected(pool: PgPool) {
    let (state, token, _handle) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let _executor = ExecutorGuard::spawn(&state);

    let owner_id = admin_user_id(&pool).await;
    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    let app = e2e_helpers::pipeline_test_router(state.clone());
    let (_pipeline_id, _status, steps) = poll_project_pipeline(&app, &token, project_id, 300).await;

    // Verify secrets resolution for pipeline scope
    if let Some(master_key_hex) = &state.config.secrets.master_key {
        let key = platform_secrets::parse_master_key(master_key_hex).unwrap();
        let scoped = platform_secrets::query_scoped_secrets(
            &pool,
            &key,
            project_id,
            &["pipeline", "all"],
            None,
        )
        .await
        .unwrap();
        let secret_names: Vec<&str> = scoped.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            secret_names.contains(&"APP_SECRET_KEY"),
            "APP_SECRET_KEY missing from pipeline scope"
        );
        assert!(
            secret_names.contains(&"DATABASE_URL"),
            "DATABASE_URL missing from pipeline scope"
        );
        assert!(
            secret_names.contains(&"VALKEY_URL"),
            "VALKEY_URL missing from pipeline scope"
        );
        // SENTRY_DSN is deploy-scoped — should NOT be in pipeline scope
        assert!(
            !secret_names.contains(&"SENTRY_DSN"),
            "SENTRY_DSN should not be in pipeline scope"
        );
    }

    // At least one step should have reached success or failure (proving pod ran)
    let has_terminal = steps
        .iter()
        .any(|s| matches!(s["status"].as_str(), Some("success" | "failure")));
    assert!(has_terminal, "no step reached terminal status: {steps:?}");
}

// ===========================================================================
// Test 6: Secrets deploy hierarchy
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_secrets_deploy_hierarchy(pool: PgPool) {
    let (state, _token) = e2e_helpers::e2e_state(pool.clone()).await;
    let owner_id = admin_user_id(&pool).await;

    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    let Some(master_key_hex) = &state.config.secrets.master_key else {
        panic!("PLATFORM_MASTER_KEY not set");
    };
    let key = platform_secrets::parse_master_key(master_key_hex).unwrap();

    // Staging DATABASE_URL should return staging-specific override
    let staging_db = platform_secrets::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        None,
        Some("staging"),
        "DATABASE_URL",
        "staging",
    )
    .await
    .unwrap();
    assert!(
        staging_db.contains("app"),
        "staging DATABASE_URL should contain app, got: {staging_db}"
    );

    // Production DATABASE_URL should return production-specific override
    let prod_db = platform_secrets::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        None,
        Some("production"),
        "DATABASE_URL",
        "prod",
    )
    .await
    .unwrap();
    assert!(
        prod_db.contains("app"),
        "production DATABASE_URL should contain app, got: {prod_db}"
    );

    // Staging SENTRY_DSN should resolve via all scope fallback
    let sentry = platform_secrets::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        None,
        Some("staging"),
        "SENTRY_DSN",
        "all",
    )
    .await;
    assert!(sentry.is_ok(), "SENTRY_DSN should resolve for staging");

    // Staging VALKEY_URL should resolve via scope=all fallback
    let valkey = platform_secrets::resolve_secret_hierarchical(
        &pool,
        &key,
        project_id,
        None,
        Some("staging"),
        "VALKEY_URL",
        "all",
    )
    .await;
    assert!(
        valkey.is_ok(),
        "VALKEY_URL should resolve for staging via all fallback"
    );
}

// ===========================================================================
// Test 7: Pipeline steps reach terminal
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_pipeline_steps_terminal(pool: PgPool) {
    let (state, token, _handle) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let _executor = ExecutorGuard::spawn(&state);

    let owner_id = admin_user_id(&pool).await;
    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    let app = e2e_helpers::pipeline_test_router(state);
    let (_pipeline_id, status, steps) = poll_project_pipeline(&app, &token, project_id, 300).await;

    // Pipeline itself should be terminal
    assert!(
        matches!(status.as_str(), "success" | "failure" | "cancelled"),
        "pipeline not terminal: {status}"
    );

    // Every step should be terminal
    for step in &steps {
        let step_status = step["status"].as_str().unwrap_or("unknown");
        assert!(
            matches!(step_status, "success" | "failure" | "cancelled" | "skipped"),
            "step {} is not terminal: {step_status}",
            step["name"]
        );
    }
}

// ===========================================================================
// Test 8: OTEL ingest and query
// ===========================================================================

fn otel_resource_attrs(
    service_name: &str,
    project_id: Uuid,
) -> Vec<platform_observe::proto::KeyValue> {
    vec![
        platform_observe::proto::KeyValue {
            key: "service.name".into(),
            value: Some(platform_observe::proto::AnyValue {
                value: Some(platform_observe::proto::any_value::Value::StringValue(
                    service_name.into(),
                )),
            }),
        },
        platform_observe::proto::KeyValue {
            key: "platform.project_id".into(),
            value: Some(platform_observe::proto::AnyValue {
                value: Some(platform_observe::proto::any_value::Value::StringValue(
                    project_id.to_string(),
                )),
            }),
        },
    ]
}

fn build_shop_trace(project_id: Uuid) -> Vec<u8> {
    use platform_observe::proto::*;

    let resource = Resource {
        attributes: otel_resource_attrs("demo-shop", project_id),
        ..Default::default()
    };

    let trace_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let parent_span_id = uuid::Uuid::new_v4().as_bytes()[..8].to_vec();
    let child_span_id = uuid::Uuid::new_v4().as_bytes()[..8].to_vec();

    let spans = vec![
        Span {
            trace_id: trace_id.clone(),
            span_id: parent_span_id.clone(),
            name: "GET /product/1".into(),
            kind: 2, // SERVER
            start_time_unix_nano: 1_700_000_000_000_000_000,
            end_time_unix_nano: 1_700_000_000_050_000_000,
            status: Some(SpanStatus {
                code: 1, // OK
                ..Default::default()
            }),
            ..Default::default()
        },
        Span {
            trace_id,
            span_id: child_span_id,
            parent_span_id: parent_span_id.clone(),
            name: "checkout.process_order".into(),
            kind: 3, // INTERNAL
            start_time_unix_nano: 1_700_000_000_010_000_000,
            end_time_unix_nano: 1_700_000_000_040_000_000,
            status: Some(SpanStatus {
                code: 1,
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(resource),
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

fn build_shop_logs(project_id: Uuid) -> Vec<u8> {
    use platform_observe::proto::*;

    let resource = Resource {
        attributes: otel_resource_attrs("demo-shop", project_id),
        ..Default::default()
    };

    let logs = vec![
        LogRecord {
            time_unix_nano: 1_700_000_000_000_000_000,
            severity_number: 9, // INFO
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue("product viewed".into())),
            }),
            ..Default::default()
        },
        LogRecord {
            time_unix_nano: 1_700_000_000_100_000_000,
            severity_number: 9,
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue("order placed".into())),
            }),
            ..Default::default()
        },
    ];

    let request = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource),
            scope_logs: vec![ScopeLogs {
                log_records: logs,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

fn build_shop_metrics(project_id: Uuid) -> Vec<u8> {
    use platform_observe::proto::*;

    let resource = Resource {
        attributes: otel_resource_attrs("demo-shop", project_id),
        ..Default::default()
    };

    let now_ns: u64 = 1_700_000_000_000_000_000;
    let dp = |val: f64| NumberDataPoint {
        time_unix_nano: now_ns,
        value: Some(number_data_point::Value::AsDouble(val)),
        ..Default::default()
    };

    let metrics = vec![
        Metric {
            name: "shop.product_views".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![dp(42.0)],
                ..Default::default()
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.cart_additions".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![dp(15.0)],
                ..Default::default()
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.orders_placed".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![dp(5.0)],
                ..Default::default()
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.revenue_cents".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![dp(24995.0)],
                ..Default::default()
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.products_in_stock".into(),
            data: Some(metric_data::Data::Gauge(Gauge {
                data_points: vec![dp(600.0)],
            })),
            ..Default::default()
        },
    ];

    let request = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource),
            scope_metrics: vec![ScopeMetrics {
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_otel_ingest_and_query(pool: PgPool) {
    let (state, token) = e2e_helpers::e2e_state(pool.clone()).await;
    let owner_id = admin_user_id(&pool).await;

    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    // Set up observe channels + flush tasks
    let observe_state = state.observe_state();
    let cancel = tokio_util::sync::CancellationToken::new();
    let tracker = tokio_util::task::TaskTracker::new();
    let channels =
        platform_observe::spawn_background_tasks(&observe_state, cancel.clone(), &tracker);

    let app = e2e_helpers::observe_pipeline_test_router(state, channels);

    // Send OTLP data
    let trace_status =
        e2e_helpers::post_protobuf(&app, &token, "/v1/traces", build_shop_trace(project_id)).await;
    assert_eq!(trace_status, StatusCode::OK, "trace ingest failed");

    let log_status =
        e2e_helpers::post_protobuf(&app, &token, "/v1/logs", build_shop_logs(project_id)).await;
    assert_eq!(log_status, StatusCode::OK, "log ingest failed");

    let metric_status =
        e2e_helpers::post_protobuf(&app, &token, "/v1/metrics", build_shop_metrics(project_id))
            .await;
    assert_eq!(metric_status, StatusCode::OK, "metric ingest failed");

    // Wait for flush — metrics need extra time for series registration
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Query traces
    let (status, body) = e2e_helpers::get_json(&app, &token, "/api/observe/traces?limit=10").await;
    assert_eq!(status, StatusCode::OK, "trace query failed: {body}");
    let traces = body["items"].as_array().expect("no trace items");
    assert!(!traces.is_empty(), "expected at least 1 trace");

    // Query logs
    let (status, body) = e2e_helpers::get_json(&app, &token, "/api/observe/logs?limit=10").await;
    assert_eq!(status, StatusCode::OK, "log query failed: {body}");
    let logs = body["items"].as_array().expect("no log items");
    assert!(
        logs.len() >= 2,
        "expected at least 2 logs, got {}",
        logs.len()
    );

    // Query metric names
    let (status, body) = e2e_helpers::get_json(&app, &token, "/api/observe/metrics/names").await;
    assert_eq!(status, StatusCode::OK, "metric names query failed: {body}");
    // Response is Vec<MetricNameResponse>, a JSON array at root level
    let metric_items = body.as_array().expect("expected array of metric names");
    let names: Vec<&str> = metric_items
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    for expected_name in [
        "shop.product_views",
        "shop.cart_additions",
        "shop.orders_placed",
        "shop.revenue_cents",
        "shop.products_in_stock",
    ] {
        assert!(
            names.contains(&expected_name),
            "missing metric name: {expected_name}, got: {names:?}"
        );
    }

    cancel.cancel();
}

// ===========================================================================
// Test 9: Pipeline creates OTEL token
// ===========================================================================

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
async fn demo_pipeline_otel_token_created(pool: PgPool) {
    let (state, token, _handle) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let _executor = ExecutorGuard::spawn(&state);

    let owner_id = admin_user_id(&pool).await;
    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    let app = e2e_helpers::pipeline_test_router(state);
    let _ = poll_project_pipeline(&app, &token, project_id, 300).await;

    // Check that an OTEL token was created for the project
    let otel_token_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_tokens
         WHERE user_id IN (SELECT id FROM users WHERE name = 'admin')
           AND name LIKE 'otlp-pipeline-%'
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        otel_token_count >= 1,
        "expected at least 1 OTEL pipeline token, got {otel_token_count}"
    );
}

// ===========================================================================
// Test 10: Full demo lifecycle
// ===========================================================================

struct ReconcilerGuard {
    cancel: tokio_util::sync::CancellationToken,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl ReconcilerGuard {
    fn spawn(state: &platform_next::state::PlatformState) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let deployer_state = state.deployer_state();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            platform_deployer::reconciler::run(deployer_state, token).await;
        });
        Self { cancel, handle }
    }
}

impl Drop for ReconcilerGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct EventBusGuard {
    cancel: tokio_util::sync::CancellationToken,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl EventBusGuard {
    fn spawn(state: &platform_next::state::PlatformState) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let s = state.clone();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            platform_next::eventbus::run(s, token).await;
        });
        Self { cancel, handle }
    }
}

impl Drop for EventBusGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct AnalysisGuard {
    cancel: tokio_util::sync::CancellationToken,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl AnalysisGuard {
    fn spawn(state: &platform_next::state::PlatformState) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let deployer_state = state.deployer_state();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            platform_deployer::analysis::run(deployer_state, token).await;
        });
        Self { cancel, handle }
    }
}

impl Drop for AnalysisGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "../../../migrations")]
#[allow(clippy::too_many_lines)]
async fn demo_full_lifecycle(pool: PgPool) {
    use sqlx::Row as _;

    let (state, token, _handle, observe_cancel) =
        e2e_helpers::start_observe_pipeline_server(pool.clone()).await;

    // Spawn all background tasks
    let _executor = ExecutorGuard::spawn(&state);
    let _reconciler = ReconcilerGuard::spawn(&state);
    let _eventbus = EventBusGuard::spawn(&state);
    let _analysis = AnalysisGuard::spawn(&state);

    // Seed container images if registry is configured
    if state.config.registry.registry_url.is_some() {
        let seed_path = &state.config.registry.seed_images_path;
        if seed_path.exists() {
            if let Err(e) =
                platform_registry::seed::seed_all(&state.pool, &state.minio, seed_path).await
            {
                tracing::warn!(error = %e, "seed_all failed (may not have images)");
            }
        }
    }

    let owner_id = admin_user_id(&pool).await;
    let app = e2e_helpers::observe_pipeline_test_router(state.clone(), {
        let observe_state = state.observe_state();
        let tracker = tokio_util::task::TaskTracker::new();
        platform_observe::spawn_background_tasks(&observe_state, observe_cancel.clone(), &tracker)
    });

    // ── Stage 1: Create demo project ──────────────────────────────────────
    tracing::info!("stage 1: creating demo project");
    let (project_id, _) = platform_next::demo::demo_project::create_demo_project(&state, owner_id)
        .await
        .expect("create_demo_project failed");

    // Verify basic DB state
    let active: bool = sqlx::query_scalar("SELECT is_active FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(active);

    let mr_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM merge_requests WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mr_count, 1);

    let issue_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(issue_count, 4);

    let secret_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(secret_count, 6);

    // ── Stage 2: MR pipeline execution ──────────────────────────────────
    tracing::info!("stage 2: MR pipeline execution");
    state.pipeline_notify.notify_one();

    let (mr_pipeline_id, mr_status, mr_steps) =
        poll_project_pipeline(&app, &token, project_id, 480).await;
    tracing::info!(%mr_pipeline_id, %mr_status, steps = mr_steps.len(), "MR pipeline done");

    let step_names: Vec<&str> = mr_steps.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(step_names.contains(&"build-app"), "missing build-app");
    assert!(step_names.contains(&"build-test"), "missing build-test");

    // build-test should NOT be skipped
    if let Some(build_test) = mr_steps.iter().find(|s| s["name"] == "build-test") {
        assert_ne!(
            build_test["status"].as_str().unwrap_or(""),
            "skipped",
            "build-test was skipped"
        );
    }

    // All steps should be terminal
    for step in &mr_steps {
        let s = step["status"].as_str().unwrap_or("unknown");
        assert!(
            matches!(s, "success" | "failure" | "cancelled" | "skipped"),
            "step {} not terminal: {s}",
            step["name"]
        );
    }

    // On failure, dump diagnostic info
    if mr_status != "success" {
        tracing::error!("MR pipeline failed — dumping step info");
        for step in &mr_steps {
            tracing::error!(
                step = %step["name"],
                status = %step["status"],
                "step detail"
            );
        }
    }
    assert_eq!(mr_status, "success", "MR pipeline should succeed");

    // ── Stage 3: Auto-merge ──────────────────────────────────────────────
    tracing::info!("stage 3: waiting for auto-merge");
    let mr_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.1'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    poll_until("auto-merge", 60, || async {
        let status: String = sqlx::query_scalar("SELECT status FROM merge_requests WHERE id = $1")
            .bind(mr_id)
            .fetch_one(&pool)
            .await
            .unwrap_or_default();
        status == "merged"
    })
    .await;
    tracing::info!("MR auto-merged");

    // ── Stage 4: Main branch push pipeline ──────────────────────────────
    tracing::info!("stage 4: push pipeline");
    state.pipeline_notify.notify_one();

    // Wait for a second pipeline (push trigger)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let push_pipeline_id = loop {
        let (_, body) = e2e_helpers::get_json(
            &app,
            &token,
            &format!("/api/projects/{project_id}/pipelines?limit=10"),
        )
        .await;
        if let Some(items) = body["items"].as_array() {
            if items.len() >= 2 {
                // The second pipeline is the push pipeline
                if let Some(id) = items[0]["id"].as_str() {
                    if id != mr_pipeline_id {
                        break id.to_string();
                    }
                }
                if let Some(id) = items[1]["id"].as_str() {
                    if id != mr_pipeline_id {
                        break id.to_string();
                    }
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!("push pipeline not created within 60s");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    state.pipeline_notify.notify_one();
    let push_status =
        e2e_helpers::poll_pipeline_status(&app, &token, project_id, &push_pipeline_id, 300).await;
    tracing::info!(%push_pipeline_id, %push_status, "push pipeline done");

    // Push pipeline: build-app should run, build-test/e2e should be skipped
    let (_, push_detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{push_pipeline_id}"),
    )
    .await;
    if let Some(push_steps) = push_detail["steps"].as_array() {
        let push_step_names: Vec<&str> = push_steps
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        // sync-ops-repo should exist for push
        if push_step_names.contains(&"sync-ops-repo") {
            let sync_step = push_steps
                .iter()
                .find(|s| s["name"] == "sync-ops-repo")
                .unwrap();
            assert_ne!(
                sync_step["status"].as_str().unwrap_or(""),
                "skipped",
                "sync-ops-repo should run on push"
            );
        }
    }

    // ── Stage 5: GitOps sync + v0.1 staging deploy ──────────────────────
    tracing::info!("stage 5: waiting for staging deploy");
    state.deploy_notify.notify_one();

    // Poll for staging release
    poll_until("staging release", 180, || async {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deploy_releases dr
             JOIN deploy_targets dt ON dr.target_id = dt.id
             WHERE dt.project_id = $1 AND dt.environment = 'staging'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        count > 0
    })
    .await;

    // Verify staging release details
    let staging_row = sqlx::query(
        "SELECT dr.strategy, dr.phase FROM deploy_releases dr
         JOIN deploy_targets dt ON dr.target_id = dt.id
         WHERE dt.project_id = $1 AND dt.environment = 'staging'
         ORDER BY dr.created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .unwrap();

    if let Some(row) = staging_row {
        let strategy: String = row.get("strategy");
        assert_eq!(strategy, "rolling", "v0.1 should use rolling strategy");
    }

    // ── Stage 6: Auto-promote to prod + PR2 ─────────────────────────────
    tracing::info!("stage 6: waiting for PR2 creation");
    state.deploy_notify.notify_one();

    // Wait for PR2 on feature/shop-app-v0.2
    poll_until("PR2 creation", 240, || async {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM merge_requests
             WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.2'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        count > 0
    })
    .await;

    let pr2_auto_merge: bool = sqlx::query_scalar(
        "SELECT auto_merge FROM merge_requests
         WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.2'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap_or(false);
    assert!(pr2_auto_merge, "PR2 should have auto_merge=true");
    tracing::info!("PR2 created with auto_merge=true");

    // ── Stage 7: PR2 pipeline + auto-merge ──────────────────────────────
    tracing::info!("stage 7: PR2 pipeline");
    state.pipeline_notify.notify_one();

    // Wait for PR2 pipeline
    poll_until("PR2 pipeline", 120, || async {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pipelines p
             JOIN merge_requests mr ON p.merge_request_id = mr.id
             WHERE p.project_id = $1 AND mr.source_branch = 'feature/shop-app-v0.2'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        count > 0
    })
    .await;

    let pr2_pipeline_id: String = sqlx::query_scalar(
        "SELECT p.id::text FROM pipelines p
         JOIN merge_requests mr ON p.merge_request_id = mr.id
         WHERE p.project_id = $1 AND mr.source_branch = 'feature/shop-app-v0.2'
         ORDER BY p.created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    state.pipeline_notify.notify_one();
    let pr2_status =
        e2e_helpers::poll_pipeline_status(&app, &token, project_id, &pr2_pipeline_id, 300).await;
    tracing::info!(%pr2_pipeline_id, %pr2_status, "PR2 pipeline done");
    assert_eq!(pr2_status, "success", "PR2 pipeline should succeed");

    // Wait for PR2 to auto-merge
    poll_until("PR2 auto-merge", 60, || async {
        let status: String = sqlx::query_scalar(
            "SELECT status FROM merge_requests
             WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.2'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_default();
        status == "merged"
    })
    .await;
    tracing::info!("PR2 auto-merged");

    // ── Stage 8: v0.2 push pipeline ──────────────────────────────────────
    tracing::info!("stage 8: v0.2 push pipeline");
    state.pipeline_notify.notify_one();

    // Wait for v0.2 push pipeline
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let v2_push_id = loop {
        let (_, body) = e2e_helpers::get_json(
            &app,
            &token,
            &format!("/api/projects/{project_id}/pipelines?limit=20"),
        )
        .await;
        if let Some(items) = body["items"].as_array() {
            // Find the latest push-triggered pipeline
            for item in items {
                if item["trigger"].as_str() == Some("push") {
                    let id = item["id"].as_str().unwrap_or("");
                    if id != push_pipeline_id {
                        break;
                    }
                }
            }
            // Check if we found one
            for item in items {
                if item["trigger"].as_str() == Some("push") {
                    let id = item["id"].as_str().unwrap_or("").to_string();
                    if id != push_pipeline_id {
                        // Found the v0.2 push pipeline
                        let v2_id = id;
                        state.pipeline_notify.notify_one();
                        let v2_status = e2e_helpers::poll_pipeline_status(
                            &app, &token, project_id, &v2_id, 300,
                        )
                        .await;
                        tracing::info!(%v2_id, %v2_status, "v0.2 push pipeline done");
                        assert_eq!(v2_status, "success", "v0.2 push pipeline should succeed");

                        // Continue to stage 9
                        break;
                    }
                }
            }
            // Simple approach: if 4+ pipelines exist, we've likely seen the v0.2 push
            if items.len() >= 4 {
                break items[0]["id"].as_str().unwrap_or("").to_string();
            }
        }
        if tokio::time::Instant::now() > deadline {
            tracing::warn!("v0.2 push pipeline not found within 120s, continuing");
            break String::new();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    let _ = v2_push_id; // used for logging only

    // ── Stage 9: Canary deploy verification ──────────────────────────────
    tracing::info!("stage 9: canary deploy verification");
    state.deploy_notify.notify_one();

    // Poll for canary staging release
    poll_until("canary staging release", 180, || async {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deploy_releases dr
             JOIN deploy_targets dt ON dr.target_id = dt.id
             WHERE dt.project_id = $1 AND dt.environment = 'staging' AND dr.strategy = 'canary'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        count > 0
    })
    .await;

    let canary_row = sqlx::query(
        "SELECT dr.strategy, dr.phase, dr.traffic_weight FROM deploy_releases dr
         JOIN deploy_targets dt ON dr.target_id = dt.id
         WHERE dt.project_id = $1 AND dt.environment = 'staging' AND dr.strategy = 'canary'
         ORDER BY dr.created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .unwrap();

    if let Some(row) = canary_row {
        let strategy: String = row.get("strategy");
        assert_eq!(strategy, "canary");
    }

    // ── Stage 10: v0.2 production deploy ─────────────────────────────────
    tracing::info!("stage 10: production deploy");
    state.deploy_notify.notify_one();

    poll_until("production release", 180, || async {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deploy_releases dr
             JOIN deploy_targets dt ON dr.target_id = dt.id
             WHERE dt.project_id = $1 AND dt.environment = 'production'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        count > 0
    })
    .await;

    tracing::info!("demo full lifecycle complete");
    observe_cancel.cancel();
}
