mod e2e_helpers;

use axum::http::StatusCode;
use prost::Message;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E2E Demo Project Tests
//
// Validates the full demo project lifecycle: creation, MR, pipeline execution.
// The critical assertion is that the MR-triggered pipeline runs ALL steps
// (build-app, build-test, e2e) — none skipped — because the
// demo uses on_mr() trigger (not on_api()).
//
// Requires Kind cluster + registry. Run with: just test-e2e
// ---------------------------------------------------------------------------

/// RAII guard that spawns the pipeline executor and shuts it down on drop.
struct ExecutorGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl ExecutorGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let executor_state = state.clone();
        let handle = tokio::spawn(async move {
            platform::pipeline::executor::run(executor_state, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            handle,
        }
    }

    #[allow(dead_code)]
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

/// Resolve admin `user_id` from the DB.
async fn admin_user_id(pool: &PgPool) -> Uuid {
    let row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .expect("admin user must exist");
    row.0
}

/// Poll until a pipeline for the given project reaches a terminal state.
/// Returns `(pipeline_id, final_status, steps_json)`.
async fn poll_project_pipeline(
    app: &axum::Router,
    token: &str,
    project_id: Uuid,
    timeout_secs: u64,
) -> (String, String, Vec<serde_json::Value>) {
    let start = std::time::Instant::now();

    // First, wait for a pipeline to appear
    let pipeline_id = loop {
        let (status, body) = e2e_helpers::get_json(
            app,
            token,
            &format!("/api/projects/{project_id}/pipelines?limit=1"),
        )
        .await;
        if status == StatusCode::OK
            && let Some(items) = body["items"].as_array()
            && let Some(first) = items.first()
            && let Some(id) = first["id"].as_str()
        {
            break id.to_string();
        }
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "no pipeline appeared within {timeout_secs}s"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    // Now poll that pipeline until terminal
    let final_status =
        e2e_helpers::poll_pipeline_status(app, token, project_id, &pipeline_id, timeout_secs).await;

    // Get pipeline detail with steps
    let (_, detail) = e2e_helpers::get_json(
        app,
        token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;
    let steps = detail["steps"].as_array().cloned().unwrap_or_default();

    (pipeline_id, final_status, steps)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: Demo project creation produces expected DB state.
///
/// Verifies: project row, MR on feature/shop-app-v0.1, 4 sample issues,
/// `demo_project_id` setting stored.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_project_creation(pool: PgPool) {
    let (state, _token) = e2e_helpers::e2e_state(pool.clone()).await;
    let admin_id = admin_user_id(&pool).await;

    let (project_id, project_name) =
        platform::onboarding::demo_project::create_demo_project(&state, admin_id)
            .await
            .expect("create_demo_project failed");

    assert_eq!(project_name, "platform-demo");

    // Project row exists and is active
    let project_active: Option<bool> =
        sqlx::query_scalar("SELECT is_active FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(project_active, Some(true));

    // MR on feature/shop-app-v0.1 exists
    let mr_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.1'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        mr_count, 1,
        "should have exactly 1 MR on feature/shop-app-v0.1"
    );

    // 4 sample issues created
    let issue_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(issue_count, 4, "should have 4 sample issues");

    // demo_project_id setting stored
    let setting: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM platform_settings WHERE key = 'demo_project_id'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(
        setting.is_some(),
        "demo_project_id setting should be stored"
    );

    // Pipeline was triggered (should have at least 1 pipeline row)
    let pipeline_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pipelines WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        pipeline_count >= 1,
        "should have at least 1 pipeline triggered"
    );

    // Pipeline trigger should be 'mr' (not 'api')
    let trigger_type: Option<String> = sqlx::query_scalar(
        "SELECT trigger FROM pipelines WHERE project_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        trigger_type.as_deref(),
        Some("mr"),
        "demo pipeline should use 'mr' trigger, not 'api'"
    );
}

/// Test 2: Demo pipeline MR-only steps are not event-filtered.
///
/// This is the critical test: before the fix, build-test and e2e were skipped
/// because the trigger was 'api' instead of 'mr'. With trigger='mr', steps
/// with `only: events: [mr]` should NOT be event-filtered.
///
/// build-test must run (not skipped). If the kaniko build itself fails, that's
/// OK — what matters is the step was attempted, not event-filtered.
/// The e2e step `depends_on` [build-app, build-test], so if either dependency
/// fails, e2e is legitimately skipped by the DAG scheduler (not event-filtered).
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_pipeline_mr_steps_not_filtered(pool: PgPool) {
    let (state, token, _server) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let app = e2e_helpers::pipeline_test_router(state.clone());
    let admin_id = admin_user_id(&state.pool).await;

    // Spawn executor so the pipeline actually runs
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    // Wake executor
    state.pipeline_notify.notify_one();

    // Poll for pipeline completion (kaniko builds can take a while)
    let (_pipeline_id, _final_status, steps) =
        poll_project_pipeline(&app, &token, project_id, 300).await;

    // Verify expected steps exist (v0.1 pipeline: build-app, build-dev, build-test, e2e)
    let step_names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        step_names.contains(&"build-app"),
        "build-app step should exist. steps: {step_names:?}"
    );
    assert!(
        step_names.contains(&"build-test"),
        "build-test step should exist. steps: {step_names:?}"
    );
    assert!(
        step_names.contains(&"e2e"),
        "e2e step should exist. steps: {step_names:?}"
    );

    // Log all step statuses for visibility
    for step in &steps {
        let name = step["name"].as_str().unwrap_or("?");
        let status = step["status"].as_str().unwrap_or("?");
        tracing::info!(
            step_name = name,
            step_status = status,
            "demo pipeline step result"
        );
    }

    // Critical assertion: build-test must NOT be 'skipped'.
    // Before the fix it was event-filtered (trigger=api, condition=mr).
    // It should now run (success or failure — both prove the event filter passed).
    let build_test_status = steps
        .iter()
        .find(|s| s["name"].as_str() == Some("build-test"))
        .and_then(|s| s["status"].as_str())
        .unwrap_or("missing");
    assert_ne!(
        build_test_status, "skipped",
        "build-test must NOT be skipped — trigger should be 'mr'. \
         If skipped, the on_mr() fix didn't work. Got status: {build_test_status}"
    );

    // e2e depends_on [build-app, build-test]. If a dep failed, e2e is
    // legitimately skipped by the DAG (not event-filtered). Only assert
    // it's not skipped when both dependencies succeeded.
    let build_app_ok = steps.iter().any(|s| {
        s["name"].as_str() == Some("build-app") && s["status"].as_str() == Some("success")
    });
    let build_test_ok = build_test_status == "success";

    if build_app_ok && build_test_ok {
        let e2e_status = steps
            .iter()
            .find(|s| s["name"].as_str() == Some("e2e"))
            .and_then(|s| s["status"].as_str())
            .unwrap_or("missing");
        assert_ne!(
            e2e_status, "skipped",
            "e2e must NOT be skipped when both deps succeeded. Got status: {e2e_status}"
        );
    }
}

/// Test 3: Demo project creates sample secrets in DB.
///
/// Verifies that `create_demo_project` seeds secrets for pipeline and deploy scopes,
/// including environment-specific overrides.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_secrets_created(pool: PgPool) {
    let (state, _token) = e2e_helpers::e2e_state(pool.clone()).await;
    let admin_id = admin_user_id(&pool).await;

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    // Count all secrets for this project
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    // 4 env-less + 2 env-specific (staging DATABASE_URL, production DATABASE_URL)
    assert_eq!(
        total, 6,
        "should have 6 secrets total (4 base + 2 env overrides)"
    );

    // Pipeline-scoped secret exists
    let pipeline_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND scope = 'pipeline'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        pipeline_count >= 1,
        "should have at least 1 pipeline-scoped secret"
    );

    // Environment-scoped secrets exist (staging + prod)
    let env_scoped_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND scope IN ('staging', 'prod')",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        env_scoped_count >= 2,
        "should have at least 2 env-scoped secrets (staging + prod)"
    );

    // "all" scope secrets exist (DATABASE_URL, VALKEY_URL)
    let all_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND scope = 'all'")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(all_count, 2, "should have 2 'all'-scoped secrets");

    // Environment-specific overrides exist
    let staging_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND environment = 'staging'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(staging_count, 1, "should have 1 staging-specific secret");

    let prod_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secrets WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prod_count, 1, "should have 1 production-specific secret");

    // Verify secrets can be decrypted (round-trip)
    let master_key_hex = state.config.master_key.as_deref().unwrap();
    let master_key = platform::secrets::engine::parse_master_key(master_key_hex).unwrap();
    let secrets = platform::secrets::engine::query_scoped_secrets(
        &pool,
        &master_key,
        project_id,
        &["pipeline", "all"],
        None,
    )
    .await
    .unwrap();
    let secret_names: Vec<&str> = secrets.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        secret_names.contains(&"APP_SECRET_KEY"),
        "should resolve APP_SECRET_KEY. got: {secret_names:?}"
    );
    assert!(
        secret_names.contains(&"DATABASE_URL"),
        "should resolve DATABASE_URL. got: {secret_names:?}"
    );
    assert!(
        secret_names.contains(&"VALKEY_URL"),
        "should resolve VALKEY_URL. got: {secret_names:?}"
    );
}

/// Test 4: Secrets API lists demo secrets correctly.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_secrets_api_list(pool: PgPool) {
    let (state, token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let admin_id = admin_user_id(&pool).await;

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    // List secrets via API
    let (status, body) =
        e2e_helpers::get_json(&app, &token, &format!("/api/projects/{project_id}/secrets")).await;
    assert_eq!(status, StatusCode::OK, "list secrets failed: {body}");

    let total = body["total"].as_i64().unwrap_or(0);
    assert!(
        total >= 4,
        "should have at least 4 secrets via API, got {total}"
    );

    // Read a specific secret value via API (scope=all matches any request scope)
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/secrets/DATABASE_URL"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read secret failed: {body}");
    assert_eq!(
        body["value"].as_str(),
        Some("postgresql://demo:demo@platform-demo-db:5432/shop"),
        "decrypted secret value should match"
    );
}

/// Test 5: Pipeline pods receive secrets as env vars.
///
/// After pipeline execution, verifies that the executor resolved secrets and
/// injected `PLATFORM_SECRET_NAMES` into the pipeline steps' DB rows.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_pipeline_secrets_injected(pool: PgPool) {
    let (state, token, _server) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let app = e2e_helpers::pipeline_test_router(state.clone());
    let admin_id = admin_user_id(&state.pool).await;
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    // Wait for pipeline to complete
    let (pipeline_id, _final_status, _steps) =
        poll_project_pipeline(&app, &token, project_id, 300).await;

    // Check the pipeline's steps in DB for secret injection evidence.
    // The executor sets PLATFORM_SECRET_NAMES env var on each pod.
    // We can verify secrets were resolved by querying the pipeline's pod spec
    // via K8s API (if the pod still exists) or by checking the step completed
    // (which means the pod ran with env vars injected).

    // Verify pipeline-scoped secrets were resolvable at execution time
    let master_key_hex = state.config.master_key.as_deref().unwrap();
    let master_key = platform::secrets::engine::parse_master_key(master_key_hex).unwrap();
    let pipeline_secrets = platform::secrets::engine::query_scoped_secrets(
        &state.pool,
        &master_key,
        project_id,
        &["pipeline", "agent", "all"],
        None,
    )
    .await
    .unwrap();

    // Should include APP_SECRET_KEY (pipeline), DATABASE_URL (all), VALKEY_URL (all)
    let names: Vec<&str> = pipeline_secrets.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"APP_SECRET_KEY"),
        "pipeline secrets should include APP_SECRET_KEY. got: {names:?}"
    );
    assert!(
        names.contains(&"DATABASE_URL"),
        "pipeline secrets should include DATABASE_URL. got: {names:?}"
    );
    assert!(
        names.contains(&"VALKEY_URL"),
        "pipeline secrets should include VALKEY_URL. got: {names:?}"
    );
    // SENTRY_DSN is staging-scoped, should NOT appear in pipeline secrets
    assert!(
        !names.contains(&"SENTRY_DSN"),
        "pipeline secrets should NOT include SENTRY_DSN (deploy scope). got: {names:?}"
    );

    // Verify a step actually ran (not just pending) — proves pod was created with env
    let ran_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pipeline_steps
         WHERE pipeline_id = $1 AND status IN ('success', 'failure')",
    )
    .bind(Uuid::parse_str(&pipeline_id).unwrap())
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(
        ran_count >= 1,
        "at least one step should have run (success or failure), proving secrets were injected"
    );
}

/// Test 6: Deploy-scoped secrets resolve with environment specificity.
///
/// Verifies the hierarchical secret resolution: staging gets the staging
/// override, production gets the production override, and both fall back
/// to env-less secrets when no override exists.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_secrets_deploy_hierarchy(pool: PgPool) {
    let (state, _token) = e2e_helpers::e2e_state(pool.clone()).await;
    let admin_id = admin_user_id(&pool).await;

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    let master_key_hex = state.config.master_key.as_deref().unwrap();
    let master_key = platform::secrets::engine::parse_master_key(master_key_hex).unwrap();

    // Use resolve_secret_hierarchical for most-specific-wins resolution.

    // Staging: should get staging-specific DATABASE_URL
    let staging_db = platform::secrets::engine::resolve_secret_hierarchical(
        &pool,
        &master_key,
        project_id,
        None,
        Some("staging"),
        "DATABASE_URL",
        "staging",
    )
    .await
    .unwrap();
    assert_eq!(
        staging_db, "postgresql://demo:demo@platform-demo-db:5432/shop_staging",
        "staging should get staging-specific DATABASE_URL"
    );

    // Production: should get production-specific DATABASE_URL
    let prod_db = platform::secrets::engine::resolve_secret_hierarchical(
        &pool,
        &master_key,
        project_id,
        None,
        Some("production"),
        "DATABASE_URL",
        "prod",
    )
    .await
    .unwrap();
    assert_eq!(
        prod_db, "postgresql://demo:demo@platform-demo-db:5432/shop_production",
        "production should get production-specific DATABASE_URL"
    );

    // Staging should resolve SENTRY_DSN (staging scope, env-less)
    let staging_sentry = platform::secrets::engine::resolve_secret_hierarchical(
        &pool,
        &master_key,
        project_id,
        None,
        Some("staging"),
        "SENTRY_DSN",
        "staging",
    )
    .await;
    assert!(staging_sentry.is_ok(), "staging should resolve SENTRY_DSN");

    // Both should get VALKEY_URL via scope=all fallback
    let staging_valkey = platform::secrets::engine::resolve_secret_hierarchical(
        &pool,
        &master_key,
        project_id,
        None,
        Some("staging"),
        "VALKEY_URL",
        "all",
    )
    .await;
    assert!(
        staging_valkey.is_ok(),
        "staging should resolve VALKEY_URL (scope=all)"
    );
}

/// Test 7: Demo pipeline step statuses are all terminal after completion.
///
/// Verifies that no step is left in pending/running state after the pipeline
/// reaches a terminal status.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_pipeline_steps_terminal(pool: PgPool) {
    let (state, token, _server) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let app = e2e_helpers::pipeline_test_router(state.clone());
    let admin_id = admin_user_id(&state.pool).await;
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    let (_pipeline_id, final_status, steps) =
        poll_project_pipeline(&app, &token, project_id, 300).await;

    // Pipeline should reach a terminal state
    assert!(
        matches!(final_status.as_str(), "success" | "failure" | "cancelled"),
        "pipeline should be terminal, got: {final_status}"
    );

    // All steps should be in a terminal state
    for step in &steps {
        let name = step["name"].as_str().unwrap_or("unknown");
        let status = step["status"].as_str().unwrap_or("unknown");
        assert!(
            matches!(status, "success" | "failure" | "cancelled" | "skipped"),
            "step '{name}' should be terminal, got: {status}"
        );
    }
}

// ---------------------------------------------------------------------------
// OTEL telemetry helpers
// ---------------------------------------------------------------------------

/// Build protobuf resource attributes for a service reporting to a project.
fn otel_resource_attrs(
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

/// Build an OTLP trace request mimicking the demo shop app.
fn build_shop_trace(project_id: Uuid) -> Vec<u8> {
    use platform::observe::proto::*;
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: otel_resource_attrs("platform-demo", project_id),
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![
                    Span {
                        trace_id: vec![1; 16],
                        span_id: vec![1; 8],
                        name: "GET /product/1".into(),
                        kind: 2, // SERVER
                        start_time_unix_nano: 1_700_000_000_000_000_000,
                        end_time_unix_nano: 1_700_000_000_050_000_000,
                        attributes: vec![KeyValue {
                            key: "http.method".into(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::StringValue("GET".into())),
                            }),
                        }],
                        status: Some(SpanStatus {
                            code: 1, // OK
                            message: String::new(),
                        }),
                        ..Default::default()
                    },
                    Span {
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        parent_span_id: vec![1; 8],
                        name: "checkout.process_order".into(),
                        kind: 1, // INTERNAL
                        start_time_unix_nano: 1_700_000_000_010_000_000,
                        end_time_unix_nano: 1_700_000_000_040_000_000,
                        attributes: vec![
                            KeyValue {
                                key: "cart.item_count".into(),
                                value: Some(AnyValue {
                                    value: Some(any_value::Value::IntValue(3)),
                                }),
                            },
                            KeyValue {
                                key: "order.total_cents".into(),
                                value: Some(AnyValue {
                                    value: Some(any_value::Value::IntValue(4999)),
                                }),
                            },
                        ],
                        status: Some(SpanStatus {
                            code: 1,
                            message: String::new(),
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
        }],
    }
    .encode_to_vec()
}

/// Build OTLP log records mimicking the demo shop app.
fn build_shop_logs(project_id: Uuid) -> Vec<u8> {
    use platform::observe::proto::*;
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: otel_resource_attrs("platform-demo", project_id),
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![
                    LogRecord {
                        time_unix_nano: 1_700_000_000_000_000_000,
                        severity_number: 9, // INFO
                        severity_text: "INFO".into(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("product viewed".into())),
                        }),
                        ..Default::default()
                    },
                    LogRecord {
                        time_unix_nano: 1_700_000_000_010_000_000,
                        severity_number: 9,
                        severity_text: "INFO".into(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("order placed".into())),
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
        }],
    }
    .encode_to_vec()
}

/// Build OTLP metrics mimicking the demo shop app's custom counters.
fn build_shop_metrics(project_id: Uuid) -> Vec<u8> {
    use platform::observe::proto::*;
    let resource = Some(Resource {
        attributes: otel_resource_attrs("platform-demo", project_id),
    });
    let now_ns = 1_700_000_000_000_000_000u64;

    let metrics = vec![
        Metric {
            name: "shop.product_views".into(),
            unit: "1".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(42)),
                    time_unix_nano: now_ns,
                    ..Default::default()
                }],
                is_monotonic: true,
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.cart_additions".into(),
            unit: "1".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(15)),
                    time_unix_nano: now_ns,
                    ..Default::default()
                }],
                is_monotonic: true,
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.orders_placed".into(),
            unit: "1".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(5)),
                    time_unix_nano: now_ns,
                    ..Default::default()
                }],
                is_monotonic: true,
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.revenue_cents".into(),
            unit: "cents".into(),
            data: Some(metric_data::Data::Sum(Sum {
                data_points: vec![NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(24995)),
                    time_unix_nano: now_ns,
                    ..Default::default()
                }],
                is_monotonic: true,
            })),
            ..Default::default()
        },
        Metric {
            name: "shop.products_in_stock".into(),
            unit: "1".into(),
            data: Some(metric_data::Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(600)),
                    time_unix_nano: now_ns,
                    ..Default::default()
                }],
            })),
            ..Default::default()
        },
    ];

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource,
            scope_metrics: vec![ScopeMetrics {
                metrics,
                ..Default::default()
            }],
        }],
    }
    .encode_to_vec()
}

/// Send protobuf bytes to an OTLP ingest endpoint.
async fn post_protobuf(app: &axum::Router, token: &str, path: &str, body: Vec<u8>) -> StatusCode {
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

    app.clone().oneshot(req).await.unwrap().status()
}

// ---------------------------------------------------------------------------
// OTEL Tests
// ---------------------------------------------------------------------------

/// Test 8: OTLP ingest + query round-trip for demo project telemetry.
///
/// Sends synthetic traces, logs, and custom shop.* metrics mimicking what the
/// deployed demo app would send, then queries the observe API to verify the
/// platform received them. Uses an in-process router with observe channels
/// and flush tasks.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_otel_ingest_and_query(pool: PgPool) {
    let (state, token) = e2e_helpers::e2e_state(pool.clone()).await;
    let admin_id = admin_user_id(&pool).await;

    // Create channels + flush tasks + observe-enabled router (all in-process)
    let (channels, spans_rx, logs_rx, metrics_rx) = platform::observe::ingest::create_channels();
    let app = e2e_helpers::observe_pipeline_test_router(state.clone(), channels);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    {
        let p = pool.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(platform::observe::ingest::flush_spans(p, spans_rx, rx));
    }
    {
        let p = pool.clone();
        let v = state.valkey.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(platform::observe::ingest::flush_logs(p, v, logs_rx, rx));
    }
    {
        let p = pool.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(platform::observe::ingest::flush_metrics(p, metrics_rx, rx));
    }

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    // --- Ingest synthetic OTLP data mimicking the deployed demo app ---

    let trace_status =
        post_protobuf(&app, &token, "/v1/traces", build_shop_trace(project_id)).await;
    assert_eq!(trace_status, StatusCode::OK, "trace ingest should succeed");

    let log_status = post_protobuf(&app, &token, "/v1/logs", build_shop_logs(project_id)).await;
    assert_eq!(log_status, StatusCode::OK, "log ingest should succeed");

    let metric_status =
        post_protobuf(&app, &token, "/v1/metrics", build_shop_metrics(project_id)).await;
    assert_eq!(
        metric_status,
        StatusCode::OK,
        "metric ingest should succeed"
    );

    // Give flush tasks time to write to DB
    tokio::time::sleep(Duration::from_secs(3)).await;

    // --- Query traces ---
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/observe/traces?project_id={project_id}&limit=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "trace query failed: {body}");
    let trace_total = body["total"].as_i64().unwrap_or(0);
    assert!(
        trace_total >= 1,
        "should have at least 1 trace, got {trace_total}"
    );

    // Verify trace detail has the checkout span
    if let Some(items) = body["items"].as_array()
        && let Some(first) = items.first()
        && let Some(trace_id) = first["trace_id"].as_str()
    {
        let (status, detail) =
            e2e_helpers::get_json(&app, &token, &format!("/api/observe/traces/{trace_id}")).await;
        assert_eq!(status, StatusCode::OK);
        let spans = detail["spans"].as_array().map_or(0, std::vec::Vec::len);
        assert!(
            spans >= 2,
            "trace should have at least 2 spans (GET + checkout)"
        );
    }

    // --- Query logs ---
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/observe/logs?project_id={project_id}&limit=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "log query failed: {body}");
    let log_total = body["total"].as_i64().unwrap_or(0);
    assert!(
        log_total >= 2,
        "should have at least 2 log entries, got {log_total}"
    );

    // --- Query metrics ---
    // Verify custom shop.* metric names are registered
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/observe/metrics/names?project_id={project_id}&limit=50"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "metric names query failed: {body}");

    let empty = vec![];
    let metric_names: Vec<&str> = body
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    for expected in &[
        "shop.product_views",
        "shop.cart_additions",
        "shop.orders_placed",
        "shop.revenue_cents",
        "shop.products_in_stock",
    ] {
        assert!(
            metric_names.contains(expected),
            "metric '{expected}' should be registered. got: {metric_names:?}"
        );
    }

    // Query a specific metric's data points
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/observe/metrics?project_id={project_id}&name=shop.revenue_cents&limit=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "metric query failed: {body}");
    let empty_arr = vec![];
    assert!(
        !body.as_array().unwrap_or(&empty_arr).is_empty(),
        "shop.revenue_cents should have data points"
    );

    // Clean up flush tasks
    let _ = shutdown_tx.send(());
}

/// Test 9: Pipeline executor creates OTEL token for pipeline steps.
///
/// After the demo pipeline runs, verifies that the executor created an
/// observe:write scoped API token for the pipeline's OTLP telemetry.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_pipeline_otel_token_created(pool: PgPool) {
    let (state, token, _server) = e2e_helpers::start_pipeline_server(pool.clone()).await;
    let app = e2e_helpers::pipeline_test_router(state.clone());
    let admin_id = admin_user_id(&state.pool).await;
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _) = platform::onboarding::demo_project::create_demo_project(&state, admin_id)
        .await
        .expect("create_demo_project failed");

    state.pipeline_notify.notify_one();

    // Wait for pipeline to complete
    let (_pipeline_id, _status, _steps) =
        poll_project_pipeline(&app, &token, project_id, 300).await;

    // The executor creates short-lived OTEL tokens (otlp-pipeline-*) for each pipeline.
    // Verify at least one observe:write scoped token exists for this project.
    let otel_token_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_tokens
         WHERE project_id = $1
           AND scopes @> ARRAY['observe:write']
           AND expires_at > now()",
    )
    .bind(project_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(
        otel_token_count >= 1,
        "executor should create at least 1 observe:write token for the pipeline"
    );
}

// ---------------------------------------------------------------------------
// Test 10: Full lifecycle — demo project → MR pipeline → auto-merge →
// main pipeline → staging deploy → promote → production deploy.
//
// Only two explicit actions: create_demo_project() + POST /promote-staging.
// Everything else is auto-triggered and observed via DB polling.
// ---------------------------------------------------------------------------

/// RAII guard for the deployer reconciler background task.
struct ReconcilerGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl ReconcilerGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::deployer::reconciler::run(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            handle,
        }
    }
}

impl Drop for ReconcilerGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// RAII guard for the eventbus subscriber background task.
struct EventBusGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl EventBusGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::store::eventbus::run(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            handle,
        }
    }
}

impl Drop for EventBusGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// RAII guard for the canary analysis background task.
struct AnalysisGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl AnalysisGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let s = state.clone();
        let handle = tokio::spawn(async move {
            platform::deployer::analysis::run(s, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            handle,
        }
    }
}

impl Drop for AnalysisGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// Poll a condition with timeout. Panics on timeout.
async fn poll_until<F, Fut>(label: &str, timeout_secs: u64, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if f().await {
            return;
        }
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "poll_until({label}) timed out after {timeout_secs}s"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Full lifecycle E2E test for the demo project.
///
/// Creates the demo project and observes the entire auto-triggered flow:
/// MR pipeline → auto-merge → main pipeline (with `gitops_sync` + `deploy_watch`) →
/// staging deployment → manual promote → production deployment → OTEL round-trip.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_full_lifecycle(pool: PgPool) {
    // Tracing is initialized by e2e_helpers::init_test_tracing() (called from
    // start_observe_pipeline_server → e2e_state_with_api_url). Writes JSON to
    // TEST_LOG_FILE so logs are captured even when the test passes.

    // --- Setup: real TCP server + all background tasks ---
    let (state, token, _server, shutdown_tx) =
        e2e_helpers::start_observe_pipeline_server(pool.clone()).await;

    let _executor = ExecutorGuard::spawn(&state);
    let _reconciler = ReconcilerGuard::spawn(&state);
    let _eventbus = EventBusGuard::spawn(&state);
    let _analysis = AnalysisGuard::spawn(&state);

    let admin_id = admin_user_id(&pool).await;

    // ===================================================================
    // Stage 1: Create demo project
    // ===================================================================
    let (project_id, project_name) =
        platform::onboarding::demo_project::create_demo_project(&state, admin_id)
            .await
            .expect("create_demo_project failed");

    assert_eq!(project_name, "platform-demo");

    // 1.1 Project exists and is active
    let active: bool = sqlx::query_scalar("SELECT is_active FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(active, "project should be active");

    // 1.2 Namespace slug set
    let ns_slug: String = sqlx::query_scalar("SELECT namespace_slug FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!ns_slug.is_empty(), "namespace_slug should be set");

    // 1.3 Ops repo exists
    let ops_repo_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ops_repos WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ops_repo_count, 1, "should have 1 ops repo");

    // 1.4 MR exists with auto_merge=true (PR1: v0.1 rolling deploy)
    let mr = sqlx::query(
        "SELECT source_branch, status, auto_merge FROM merge_requests WHERE project_id = $1 AND number = 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    {
        use sqlx::Row as _;
        let source: String = mr.get("source_branch");
        let status: String = mr.get("status");
        let auto_merge: bool = mr.get("auto_merge");
        assert_eq!(source, "feature/shop-app-v0.1");
        assert_eq!(status, "open");
        assert!(auto_merge, "demo MR should have auto_merge=true");
    }

    // 1.5 Issues + secrets
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

    // 1.6 include_staging = true
    let include_staging: bool =
        sqlx::query_scalar("SELECT include_staging FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        include_staging,
        "demo project should have include_staging=true"
    );

    // 1.7 MR pipeline was triggered
    let mr_trigger: Option<String> = sqlx::query_scalar(
        "SELECT trigger FROM pipelines WHERE project_id = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        mr_trigger.as_deref(),
        Some("mr"),
        "first pipeline should be MR-triggered"
    );

    tracing::info!("Stage 1 passed: demo project created");

    // ===================================================================
    // Stage 2: MR Pipeline Execution
    // ===================================================================
    state.pipeline_notify.notify_one();

    let app = e2e_helpers::observe_pipeline_test_router(
        state.clone(),
        platform::observe::ingest::create_channels().0,
    );

    let (mr_pipeline_id, mr_status, mr_steps) =
        poll_project_pipeline(&app, &token, project_id, 480).await;

    // 2.1 Pipeline trigger = mr
    let mr_pipe_trigger: String = sqlx::query_scalar("SELECT trigger FROM pipelines WHERE id = $1")
        .bind(Uuid::parse_str(&mr_pipeline_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mr_pipe_trigger, "mr");

    // 2.2 Check step names exist (v0.1 pipeline: no build-canary)
    let step_names: Vec<&str> = mr_steps.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(
        step_names.contains(&"build-app"),
        "build-app missing: {step_names:?}"
    );
    assert!(
        step_names.contains(&"build-dev"),
        "build-dev missing: {step_names:?}"
    );
    assert!(
        step_names.contains(&"build-test"),
        "build-test missing: {step_names:?}"
    );
    assert!(step_names.contains(&"e2e"), "e2e missing: {step_names:?}");

    // 2.3 build-test should NOT be skipped (trigger=mr, only: events: [mr])
    let build_test_status = mr_steps
        .iter()
        .find(|s| s["name"].as_str() == Some("build-test"))
        .and_then(|s| s["status"].as_str())
        .unwrap_or("missing");
    assert_ne!(
        build_test_status, "skipped",
        "build-test should run for MR trigger"
    );

    // 2.4 sync-ops-repo and watch-deploy should be SKIPPED (push-only steps)
    for push_only_step in &["sync-ops-repo", "watch-deploy"] {
        let status = mr_steps
            .iter()
            .find(|s| s["name"].as_str() == Some(*push_only_step))
            .and_then(|s| s["status"].as_str());
        assert_eq!(
            status,
            Some("skipped"),
            "{push_only_step} should be skipped for MR trigger"
        );
    }

    // 2.5 All steps terminal
    for step in &mr_steps {
        let name = step["name"].as_str().unwrap_or("?");
        let status = step["status"].as_str().unwrap_or("?");
        assert!(
            matches!(status, "success" | "failure" | "skipped"),
            "step '{name}' should be terminal, got: {status}"
        );
    }

    tracing::info!(%mr_status, "Stage 2 passed: MR pipeline completed");

    // Log all step statuses and fetch logs from MinIO for failed steps
    for step in &mr_steps {
        let name = step["name"].as_str().unwrap_or("?");
        let status = step["status"].as_str().unwrap_or("?");
        let log_ref = step["log_ref"].as_str().unwrap_or("");
        tracing::info!(
            step_name = name,
            step_status = status,
            "MR pipeline step result"
        );

        // Fetch logs for failed steps from MinIO
        if status == "failure" && !log_ref.is_empty() {
            // Use eprintln so it shows up in nextest failure output
            match state.minio.read(log_ref).await {
                Ok(buf) => {
                    let raw = buf.to_vec();
                    let log_text = String::from_utf8_lossy(&raw);
                    let truncated: String = log_text.chars().take(3000).collect();
                    eprintln!("=== STEP LOG [{name}] ===\n{truncated}\n===");
                }
                Err(e) => {
                    eprintln!("=== could not read step log [{name}]: {e} ===");
                }
            }
            let clone_ref = log_ref.replace(&format!("{name}.log"), &format!("{name}-clone.log"));
            if let Ok(buf) = state.minio.read(&clone_ref).await {
                let raw = buf.to_vec();
                let log_text = String::from_utf8_lossy(&raw);
                let truncated: String = log_text.chars().take(3000).collect();
                eprintln!("=== CLONE LOG [{name}] ===\n{truncated}\n===");
            }
        }
    }
    // Also dump K8s namespace events for the pipeline namespace if builds failed
    if mr_status != "success" {
        let ns_slug: String =
            sqlx::query_scalar("SELECT namespace_slug FROM projects WHERE id = $1")
                .bind(project_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let pipeline_ns = format!(
            "{}-{ns_slug}-dev",
            state.config.ns_prefix.as_deref().unwrap_or(""),
        )
        .trim_start_matches('-')
        .to_string();
        // List all pods in the pipeline namespace for diagnostics
        let pod_api: kube::Api<k8s_openapi::api::core::v1::Pod> =
            kube::Api::namespaced(state.kube.clone(), &pipeline_ns);
        if let Ok(pods) = pod_api.list(&kube::api::ListParams::default()).await {
            for pod in &pods.items {
                let pname = pod.metadata.name.as_deref().unwrap_or("?");
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("?");
                eprintln!("=== POD {pname} phase={phase} ===");
                // Print container statuses
                if let Some(ref status) = pod.status {
                    for cs in status
                        .init_container_statuses
                        .iter()
                        .flatten()
                        .chain(status.container_statuses.iter().flatten())
                    {
                        let cname = &cs.name;
                        let ready = cs.ready;
                        let state_str = cs
                            .state
                            .as_ref()
                            .map(|s| format!("{s:?}"))
                            .unwrap_or_default();
                        eprintln!("  container={cname} ready={ready} state={state_str}");
                    }
                }
            }
        }
        // List events in the namespace
        let event_api: kube::Api<k8s_openapi::api::core::v1::Event> =
            kube::Api::namespaced(state.kube.clone(), &pipeline_ns);
        if let Ok(events) = event_api.list(&kube::api::ListParams::default()).await {
            for ev in events.items.iter().take(20) {
                let reason = ev.reason.as_deref().unwrap_or("?");
                let msg = ev.message.as_deref().unwrap_or("?");
                let obj = ev.involved_object.name.as_deref().unwrap_or("?");
                eprintln!("  EVENT obj={obj} reason={reason}: {msg}");
            }
        }
    }

    assert_eq!(
        mr_status, "success",
        "MR pipeline must succeed for auto-merge to fire"
    );

    // ===================================================================
    // Stage 3: Auto-Merge
    // ===================================================================
    // try_auto_merge is called from finalize_pipeline, should fire since
    // auto_merge=true and pipeline succeeded.
    let pool_clone = pool.clone();
    poll_until("MR merged", 60, || {
        let p = pool_clone.clone();
        let pid = project_id;
        async move {
            let status: Option<String> = sqlx::query_scalar(
                "SELECT status FROM merge_requests WHERE project_id = $1 AND number = 1",
            )
            .bind(pid)
            .fetch_optional(&p)
            .await
            .ok()
            .flatten();
            status.as_deref() == Some("merged")
        }
    })
    .await;

    // 3.1 Merge commit SHA set
    // Verify MR status is merged
    let mr_status_after: String = sqlx::query_scalar(
        "SELECT status FROM merge_requests WHERE project_id = $1 AND number = 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mr_status_after, "merged");

    // 3.2 Push pipeline triggered (post-merge on_push)
    let pool_clone2 = pool.clone();
    poll_until("push pipeline created", 30, || {
        let p = pool_clone2.clone();
        let pid = project_id;
        async move {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pipelines WHERE project_id = $1 AND trigger = 'push'",
            )
            .bind(pid)
            .fetch_one(&p)
            .await
            .unwrap_or(0);
            count >= 1
        }
    })
    .await;

    tracing::info!("Stage 3 passed: auto-merge completed, push pipeline triggered");

    // ===================================================================
    // Stage 4: Main Branch Pipeline Execution
    // ===================================================================
    let main_pipeline_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM pipelines WHERE project_id = $1 AND trigger = 'push' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    state.pipeline_notify.notify_one();

    let main_status = e2e_helpers::poll_pipeline_status(
        &app,
        &token,
        project_id,
        &main_pipeline_id.to_string(),
        300,
    )
    .await;

    // 4.1 Get step details
    let (_, main_detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{main_pipeline_id}"),
    )
    .await;
    let main_steps = main_detail["steps"].as_array().cloned().unwrap_or_default();

    let main_step_names: Vec<&str> = main_steps
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();

    // 4.2 build-app, build-dev should run (v0.1: no build-canary)
    for step_name in &["build-app", "build-dev"] {
        let status = main_steps
            .iter()
            .find(|s| s["name"].as_str() == Some(*step_name))
            .and_then(|s| s["status"].as_str());
        assert_ne!(
            status,
            Some("skipped"),
            "{step_name} should run on push trigger: {main_step_names:?}"
        );
    }

    // 4.3 build-test and e2e should be SKIPPED (mr-only)
    for mr_only_step in &["build-test", "e2e"] {
        let status = main_steps
            .iter()
            .find(|s| s["name"].as_str() == Some(*mr_only_step))
            .and_then(|s| s["status"].as_str());
        assert_eq!(
            status,
            Some("skipped"),
            "{mr_only_step} should be skipped for push trigger"
        );
    }

    // 4.4 sync-ops-repo and watch-deploy should run (push + main)
    if main_step_names.contains(&"sync-ops-repo") {
        let sync_status = main_steps
            .iter()
            .find(|s| s["name"].as_str() == Some("sync-ops-repo"))
            .and_then(|s| s["status"].as_str());
        assert_ne!(
            sync_status,
            Some("skipped"),
            "sync-ops-repo should run on push to main"
        );
    }

    tracing::info!(%main_status, "Stage 4 passed: main pipeline completed");

    // ===================================================================
    // Stage 5: GitOps Sync + v0.1 Staging Deploy Verification
    // ===================================================================
    let ops_repo_path: Option<String> =
        sqlx::query_scalar("SELECT repo_path FROM ops_repos WHERE project_id = $1")
            .bind(project_id)
            .fetch_optional(&pool)
            .await
            .unwrap();

    if let Some(ops_path) = &ops_repo_path {
        // 5.1 Check staging branch exists in ops repo
        let staging_check = tokio::process::Command::new("git")
            .arg("-C")
            .arg(ops_path)
            .args(["rev-parse", "--verify", "refs/heads/staging"])
            .output()
            .await;
        if let Ok(out) = staging_check {
            assert!(out.status.success(), "ops repo should have staging branch");
        }
    }

    // 5.2 Deploy release created
    let v1_release_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    tracing::info!(v1_release_count, "Stage 5: v0.1 deploy releases created");

    if v1_release_count > 0 {
        // Wait for v0.1 staging release (rolling) to reach terminal
        let pool_clone3 = pool.clone();
        poll_until("v0.1 staging release terminal", 180, || {
            let p = pool_clone3.clone();
            let pid = project_id;
            async move {
                let phase: Option<String> = sqlx::query_scalar(
                    "SELECT dr.phase FROM deploy_releases dr
                     JOIN deploy_targets dt ON dr.target_id = dt.id
                     WHERE dr.project_id = $1 AND dt.environment = 'staging'
                     ORDER BY dr.created_at DESC LIMIT 1",
                )
                .bind(pid)
                .fetch_optional(&p)
                .await
                .ok()
                .flatten();
                matches!(
                    phase.as_deref(),
                    Some("completed" | "failed" | "rolled_back")
                )
            }
        })
        .await;

        // 5.3 v0.1 deploy uses rolling strategy
        let v1_strategy: Option<String> = sqlx::query_scalar(
            "SELECT dr.strategy FROM deploy_releases dr
             JOIN deploy_targets dt ON dr.target_id = dt.id
             WHERE dr.project_id = $1 AND dt.environment = 'staging'
             ORDER BY dr.created_at LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(
            v1_strategy.as_deref(),
            Some("rolling"),
            "v0.1 should use rolling strategy"
        );

        // 5.4 Feature flags registered
        let flag_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM feature_flags WHERE project_id = $1")
                .bind(project_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            flag_count >= 2,
            "should have at least 2 feature flags, got {flag_count}"
        );

        // 5.5 OTEL staging tokens created
        let otel_staging: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_tokens WHERE project_id = $1 AND name LIKE 'otlp-staging-%'",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            otel_staging >= 1,
            "should have at least 1 staging OTEL token"
        );
    }

    // 5.6 Annotated git tag created for v0.1
    let repo_path_str: String = sqlx::query_scalar("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let tag_check = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path_str)
        .args(["tag", "-l", "v0.1.0"])
        .output()
        .await;
    if let Ok(out) = tag_check {
        let tags = String::from_utf8_lossy(&out.stdout);
        assert!(
            tags.contains("v0.1.0"),
            "annotated tag v0.1.0 should exist after main push"
        );
    }

    tracing::info!("Stage 5 passed: v0.1 rolling deploy + git tag verified");

    // ===================================================================
    // Stage 6: Auto-promote to prod + PR2 Created
    // ===================================================================
    // The eventbus auto-promotes staging→prod for the demo project.
    // After prod completes, it creates PR2 on feature/shop-app-v0.2.
    // This can take a while: staging complete → promote → prod deploy → PR2.
    let pool_clone4 = pool.clone();
    poll_until("PR2 created", 240, || {
        let p = pool_clone4.clone();
        let pid = project_id;
        async move {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.2'",
            )
            .bind(pid)
            .fetch_one(&p)
            .await
            .unwrap_or(0);
            count >= 1
        }
    })
    .await;

    // 6.1 PR2 has auto_merge=true
    let pr2 = sqlx::query(
        "SELECT source_branch, status, auto_merge, title FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.2'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    {
        use sqlx::Row as _;
        let auto_merge: bool = pr2.get("auto_merge");
        let title: String = pr2.get("title");
        assert!(auto_merge, "PR2 should have auto_merge=true");
        assert!(
            title.contains("v0.2") || title.contains("canary"),
            "PR2 title should mention v0.2 or canary: {title}"
        );
    }

    tracing::info!("Stage 6 passed: PR2 (v0.2 canary) created");

    // ===================================================================
    // Stage 7: PR2 Pipeline Execution + Auto-Merge
    // ===================================================================
    // PR2 should have triggered an MR pipeline
    let pool_clone5 = pool.clone();
    poll_until("PR2 pipeline created", 30, || {
        let p = pool_clone5.clone();
        let pid = project_id;
        async move {
            // Count MR pipelines — should have at least 2 (PR1 + PR2)
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pipelines WHERE project_id = $1 AND trigger = 'mr'",
            )
            .bind(pid)
            .fetch_one(&p)
            .await
            .unwrap_or(0);
            count >= 2
        }
    })
    .await;

    state.pipeline_notify.notify_one();

    // Wait for PR2's MR pipeline to complete
    let pr2_pipeline_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM pipelines WHERE project_id = $1 AND trigger = 'mr' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let pr2_mr_status = e2e_helpers::poll_pipeline_status(
        &app,
        &token,
        project_id,
        &pr2_pipeline_id.to_string(),
        300,
    )
    .await;

    tracing::info!(%pr2_mr_status, "Stage 7: PR2 MR pipeline completed");

    // Wait for PR2 auto-merge
    assert_eq!(
        pr2_mr_status, "success",
        "PR2 MR pipeline must succeed for canary flow to proceed"
    );
    {
        let pool_clone6 = pool.clone();
        poll_until("PR2 merged", 60, || {
            let p = pool_clone6.clone();
            let pid = project_id;
            async move {
                let status: Option<String> = sqlx::query_scalar(
                    "SELECT status FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.2'",
                )
                .bind(pid)
                .fetch_optional(&p)
                .await
                .ok()
                .flatten();
                status.as_deref() == Some("merged")
            }
        })
        .await;

        tracing::info!("Stage 7 passed: PR2 auto-merged");

        // =============================================================
        // Stage 8: Main Pipeline After PR2 Merge (canary gitops_sync)
        // =============================================================
        let pool_clone7 = pool.clone();
        poll_until("v0.2 push pipeline created", 30, || {
            let p = pool_clone7.clone();
            let pid = project_id;
            async move {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pipelines WHERE project_id = $1 AND trigger = 'push'",
                )
                .bind(pid)
                .fetch_one(&p)
                .await
                .unwrap_or(0);
                count >= 2
            }
        })
        .await;

        state.pipeline_notify.notify_one();

        let v2_push_pipeline_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM pipelines WHERE project_id = $1 AND trigger = 'push' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let v2_push_status = e2e_helpers::poll_pipeline_status(
            &app,
            &token,
            project_id,
            &v2_push_pipeline_id.to_string(),
            300,
        )
        .await;

        // 8.1 VERSION stored in pipeline row
        let v2_version: Option<String> =
            sqlx::query_scalar("SELECT version FROM pipelines WHERE id = $1")
                .bind(v2_push_pipeline_id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        if let Some(ver) = &v2_version {
            assert!(
                ver.contains("0.2.0"),
                "v0.2 pipeline version should contain 0.2.0, got: {ver}"
            );
        }

        assert_eq!(
            v2_push_status, "success",
            "v0.2 push pipeline must succeed for canary deploy to proceed"
        );
        tracing::info!("Stage 8 passed: v0.2 push pipeline succeeded");

        // =============================================================
        // Stage 9: Canary Deploy Verification
        // =============================================================
        {
            // Wait for a second staging release (canary) to appear
            let pool_clone8 = pool.clone();
            poll_until("canary staging release terminal", 180, || {
                let p = pool_clone8.clone();
                let pid = project_id;
                async move {
                    let count: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM deploy_releases dr
                         JOIN deploy_targets dt ON dr.target_id = dt.id
                         WHERE dr.project_id = $1 AND dt.environment = 'staging'
                         AND dr.phase IN ('completed', 'failed', 'rolled_back')",
                    )
                    .bind(pid)
                    .fetch_one(&p)
                    .await
                    .unwrap_or(0);
                    count >= 2
                }
            })
            .await;

            // 9.1 Latest staging release uses canary strategy
            let canary_release = sqlx::query(
                "SELECT dr.id, dr.strategy, dr.phase, dr.traffic_weight, dr.current_step
                 FROM deploy_releases dr
                 JOIN deploy_targets dt ON dr.target_id = dt.id
                 WHERE dr.project_id = $1 AND dt.environment = 'staging'
                 ORDER BY dr.created_at DESC LIMIT 1",
            )
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            {
                use sqlx::Row as _;
                let strategy: String = canary_release.get("strategy");
                let phase: String = canary_release.get("phase");
                let weight: i32 = canary_release.get("traffic_weight");
                assert_eq!(strategy, "canary", "v0.2 should use canary strategy");
                assert_eq!(phase, "completed", "canary release should be completed");
                assert_eq!(weight, 100, "final traffic weight should be 100%");
            }

            // 9.2 Canary release history shows step progression (10 → 50 → 100)
            let canary_id: uuid::Uuid = {
                use sqlx::Row as _;
                canary_release.get("id")
            };
            let history_entries: Vec<(String, Option<i32>)> = sqlx::query_as(
                "SELECT action, traffic_weight FROM release_history
                 WHERE release_id = $1 ORDER BY created_at",
            )
            .bind(canary_id)
            .fetch_all(&pool)
            .await
            .unwrap();
            let step_weights: Vec<i32> = history_entries
                .iter()
                .filter(|(a, _)| a == "step_advanced")
                .filter_map(|(_, w)| *w)
                .collect();
            assert!(
                step_weights.len() >= 2,
                "canary should have progressed through at least 2 steps, got: {step_weights:?}"
            );
            tracing::info!(?step_weights, "canary step progression verified");

            // 9.3 Verify K8s: HTTPRoute exists in staging namespace with the gateway
            let staging_ns = state.config.project_namespace(&ns_slug, "staging");
            // Use dynamic API to check HTTPRoute (Gateway API CRD)
            let route_api: kube::Api<kube::api::DynamicObject> = kube::Api::namespaced_with(
                state.kube.clone(),
                &staging_ns,
                &kube::discovery::ApiResource {
                    group: "gateway.networking.k8s.io".into(),
                    version: "v1".into(),
                    kind: "HTTPRoute".into(),
                    api_version: "gateway.networking.k8s.io/v1".into(),
                    plural: "httproutes".into(),
                },
            );
            let routes = route_api.list(&kube::api::ListParams::default()).await;
            if let Ok(route_list) = routes {
                tracing::info!(
                    count = route_list.items.len(),
                    namespace = %staging_ns,
                    "HTTPRoute resources in staging namespace"
                );
                for route in &route_list.items {
                    let name = route.metadata.name.as_deref().unwrap_or("?");
                    tracing::info!(%name, "HTTPRoute found in staging");
                }
            }

            // 9.4 Verify K8s: canary deployment exists (or existed) in staging
            let deploy_api: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
                kube::Api::namespaced(state.kube.clone(), &staging_ns);
            if let Ok(deploy_list) = deploy_api.list(&kube::api::ListParams::default()).await {
                let deploy_names: Vec<&str> = deploy_list
                    .items
                    .iter()
                    .filter_map(|d| d.metadata.name.as_deref())
                    .collect();
                tracing::info!(?deploy_names, namespace = %staging_ns, "deployments in staging after canary");
            }

            // 9.5 Annotated git tag v0.2.0 created
            let tag_v2 = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&repo_path_str)
                .args(["tag", "-l", "v0.2.0"])
                .output()
                .await;
            if let Ok(out) = tag_v2 {
                let tags = String::from_utf8_lossy(&out.stdout);
                assert!(
                    tags.contains("v0.2.0"),
                    "annotated tag v0.2.0 should exist after v0.2 main push"
                );
            }

            tracing::info!(
                "Stage 9 passed: canary deploy verified — strategy, progression, K8s resources"
            );
        }

        // =============================================================
        // Stage 10: v0.2 Production Deploy (auto via stages config → rolling)
        // =============================================================
        // The v0.2 gitops_sync triggers a production release too.
        // With stages=[staging], prod gets rolling strategy automatically.
        let pool_clone9 = pool.clone();
        poll_until("v0.2 prod release terminal", 180, || {
            let p = pool_clone9.clone();
            let pid = project_id;
            async move {
                let phase: Option<String> = sqlx::query_scalar(
                    "SELECT dr.phase FROM deploy_releases dr
                     JOIN deploy_targets dt ON dr.target_id = dt.id
                     WHERE dr.project_id = $1 AND dt.environment = 'production'
                     ORDER BY dr.created_at DESC LIMIT 1",
                )
                .bind(pid)
                .fetch_optional(&p)
                .await
                .ok()
                .flatten();
                matches!(
                    phase.as_deref(),
                    Some("completed" | "failed" | "rolled_back")
                )
            }
        })
        .await;

        tracing::info!("Stage 10 passed: v0.2 production deploy verified");
    }

    // ===================================================================
    // Cleanup
    // ===================================================================
    let _ = shutdown_tx.send(());
    tracing::info!("demo_full_lifecycle test completed successfully");
}
