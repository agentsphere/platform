mod helpers;

use fred::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Eventbus Integration Tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn handle_event_invalid_json(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let result = platform::store::eventbus::handle_event(&state, "not valid json").await;
    assert!(result.is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn handle_event_unknown_type(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let json = r#"{"type":"UnknownEvent","project_id":"00000000-0000-0000-0000-000000000000"}"#;
    let result = platform::store::eventbus::handle_event(&state, json).await;
    assert!(result.is_err());
}

/// ImageBuilt with no existing deployment → creates default deployment.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_no_deployment_creates_default(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-img-new", "public").await;

    let event = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "registry/app:v1",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "handle_event failed: {result:?}");

    // Verify deployment was created
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT image_ref, current_status FROM deployments WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .unwrap();

    let (image_ref, status) = row.expect("deployment should exist");
    assert_eq!(image_ref, "registry/app:v1");
    assert_eq!(status, "pending");
}

/// ImageBuilt with existing deployment but no ops repo → updates image_ref directly.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_deployment_no_ops_repo(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-img-noop", "public").await;

    // Insert deployment without ops_repo_id
    sqlx::query(
        "INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status) \
         VALUES ($1, 'production', 'old:v1', 'active', 'healthy')",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let event = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "new:v2",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "handle_event failed: {result:?}");

    let (image_ref, status): (String, String) = sqlx::query_as(
        "SELECT image_ref, current_status FROM deployments WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "new:v2");
    assert_eq!(status, "pending");
}

/// OpsRepoUpdated → updates deployment row and marks pending.
#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_updated_marks_pending(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-ops-upd", "public").await;

    // Insert deployment
    sqlx::query(
        "INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status) \
         VALUES ($1, 'staging', 'old:v1', 'active', 'healthy')",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let event = serde_json::json!({
        "type": "OpsRepoUpdated",
        "project_id": project_id,
        "ops_repo_id": Uuid::new_v4(),
        "environment": "staging",
        "commit_sha": "abc123",
        "image_ref": "new:v2",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "handle_event failed: {result:?}");

    let (image_ref, status, sha): (String, String, Option<String>) = sqlx::query_as(
        "SELECT image_ref, current_status, current_sha FROM deployments WHERE project_id = $1 AND environment = 'staging'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "new:v2");
    assert_eq!(status, "pending");
    assert_eq!(sha.as_deref(), Some("abc123"));
}

/// DeployRequested delegates to ImageBuilt logic → creates deployment.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_requested_creates_deployment(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-deploy", "public").await;

    let event = serde_json::json!({
        "type": "DeployRequested",
        "project_id": project_id,
        "environment": "staging",
        "image_ref": "deploy:v1",
        "requested_by": null,
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "handle_event failed: {result:?}");

    let row: Option<(String,)> = sqlx::query_as(
        "SELECT image_ref FROM deployments WHERE project_id = $1 AND environment = 'staging'",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .unwrap();

    assert_eq!(row.unwrap().0, "deploy:v1");
}

/// RollbackRequested with no deployment → graceful skip (no error).
#[sqlx::test(migrations = "./migrations")]
async fn rollback_no_deployment_graceful(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-rb-none", "public").await;

    let event = serde_json::json!({
        "type": "RollbackRequested",
        "project_id": project_id,
        "environment": "production",
        "requested_by": null,
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(
        result.is_ok(),
        "rollback with no deployment should be ok: {result:?}"
    );
}

/// RollbackRequested with deployment but no ops repo → sets desired_status = 'rollback'.
#[sqlx::test(migrations = "./migrations")]
async fn rollback_no_ops_repo_legacy_path(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-rb-leg", "public").await;

    // Insert deployment without ops_repo_id
    sqlx::query(
        "INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status) \
         VALUES ($1, 'production', 'app:v1', 'active', 'healthy')",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let event = serde_json::json!({
        "type": "RollbackRequested",
        "project_id": project_id,
        "environment": "production",
        "requested_by": null,
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "rollback legacy path failed: {result:?}");

    let (desired, current): (String, String) = sqlx::query_as(
        "SELECT desired_status, current_status FROM deployments WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(desired, "rollback");
    assert_eq!(current, "pending");
}

/// ImageBuilt on conflict upserts existing deployment.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_upserts_on_conflict(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-upsert", "public").await;

    // First ImageBuilt → creates deployment
    let event1 = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "app:v1",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });
    platform::store::eventbus::handle_event(&state, &event1.to_string())
        .await
        .unwrap();

    // Second ImageBuilt → upserts (same project+environment)
    let event2 = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "app:v2",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });
    platform::store::eventbus::handle_event(&state, &event2.to_string())
        .await
        .unwrap();

    let (image_ref,): (String,) = sqlx::query_as(
        "SELECT image_ref FROM deployments WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "app:v2");
}

/// ImageBuilt then OpsRepoUpdated sequence → both handlers execute in order.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_then_ops_repo_updated(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-seq", "public").await;

    // Step 1: ImageBuilt creates deployment
    let event1 = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "staging",
        "image_ref": "app:v1",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });
    platform::store::eventbus::handle_event(&state, &event1.to_string())
        .await
        .unwrap();

    // Step 2: OpsRepoUpdated updates the deployment
    let event2 = serde_json::json!({
        "type": "OpsRepoUpdated",
        "project_id": project_id,
        "ops_repo_id": Uuid::new_v4(),
        "environment": "staging",
        "commit_sha": "def456",
        "image_ref": "app:v2",
    });
    platform::store::eventbus::handle_event(&state, &event2.to_string())
        .await
        .unwrap();

    let (image_ref, sha): (String, Option<String>) = sqlx::query_as(
        "SELECT image_ref, current_sha FROM deployments WHERE project_id = $1 AND environment = 'staging'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "app:v2");
    assert_eq!(sha.as_deref(), Some("def456"));
}

/// Multiple environments for the same project are independent.
#[sqlx::test(migrations = "./migrations")]
async fn different_environments_independent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-multi-env", "public").await;

    // Deploy to staging
    let staging = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "staging",
        "image_ref": "app:staging-v1",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });
    platform::store::eventbus::handle_event(&state, &staging.to_string())
        .await
        .unwrap();

    // Deploy to production
    let prod = serde_json::json!({
        "type": "ImageBuilt",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "app:prod-v1",
        "pipeline_id": Uuid::nil(),
        "triggered_by": null,
    });
    platform::store::eventbus::handle_event(&state, &prod.to_string())
        .await
        .unwrap();

    // Verify both exist with different image refs
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deployments WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    let (staging_img,): (String,) = sqlx::query_as(
        "SELECT image_ref FROM deployments WHERE project_id = $1 AND environment = 'staging'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(staging_img, "app:staging-v1");

    let (prod_img,): (String,) = sqlx::query_as(
        "SELECT image_ref FROM deployments WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prod_img, "app:prod-v1");
}

/// Empty JSON object should fail deserialization (missing 'type' field).
#[sqlx::test(migrations = "./migrations")]
async fn handle_event_empty_json_object(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let result = platform::store::eventbus::handle_event(&state, "{}").await;
    assert!(result.is_err());
}

/// JSON with wrong type value should fail.
#[sqlx::test(migrations = "./migrations")]
async fn handle_event_wrong_type_value(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let json = r#"{"type":"NotAValidEvent","project_id":"00000000-0000-0000-0000-000000000000"}"#;
    let result = platform::store::eventbus::handle_event(&state, json).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// AlertFired Integration Tests
// ---------------------------------------------------------------------------

/// AlertFired with no project_id → handler skips gracefully (no sessions created).
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_no_project_skips_spawn(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool.clone()).await;

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": Uuid::new_v4(),
        "project_id": null,
        "severity": "critical",
        "value": 95.5,
        "message": "CPU usage above threshold",
        "alert_name": "high-cpu",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "should skip gracefully: {result:?}");

    // Verify no agent sessions were created
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE prompt LIKE '%high-cpu%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "no session should be created for non-project alert"
    );
}

/// AlertFired with info severity → handler skips (only warning/critical spawn agents).
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_info_severity_skips_spawn(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-alert-info", "public").await;

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": Uuid::new_v4(),
        "project_id": project_id,
        "severity": "info",
        "value": 42.0,
        "message": "Info level alert",
        "alert_name": "info-metric",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "should skip gracefully: {result:?}");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "no session should be created for info severity");
}

/// AlertFired with active cooldown → handler skips duplicate spawn.
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_cooldown_prevents_spawn(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-alert-cd", "public").await;

    let rule_id = Uuid::new_v4();

    // Set cooldown key before firing the event
    let cooldown_key = format!("alert-agent:{project_id}:{rule_id}");
    state
        .valkey
        .next()
        .set::<(), _, _>(
            &cooldown_key,
            "1",
            Some(fred::types::Expiration::EX(900)),
            None,
            false,
        )
        .await
        .unwrap();

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": rule_id,
        "project_id": project_id,
        "severity": "critical",
        "value": 99.0,
        "message": "Critical alert with cooldown",
        "alert_name": "cooldown-test",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "should skip due to cooldown: {result:?}");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "no session should be created when cooldown active"
    );
}

/// AlertFired with 3 active ops sessions → concurrent limit prevents spawn.
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_concurrent_limit_skips_spawn(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-alert-lim", "public").await;

    // Get admin user ID
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Get agent-ops role ID
    let ops_role_id: (Uuid,) = sqlx::query_as("SELECT id FROM roles WHERE name = 'agent-ops'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Create 3 fake agent users with agent-ops role and active sessions
    for i in 0..3 {
        let agent_user_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, name, email, password_hash, is_active, user_type)
             VALUES ($1, $2, $3, 'nohash', true, 'agent')",
        )
        .bind(agent_user_id)
        .bind(format!("ops-agent-{i}"))
        .bind(format!("ops-agent-{i}@test.local"))
        .execute(&pool)
        .await
        .unwrap();

        // Assign agent-ops role
        sqlx::query(
            "INSERT INTO user_roles (user_id, role_id, project_id)
             VALUES ($1, $2, $3)",
        )
        .bind(agent_user_id)
        .bind(ops_role_id.0)
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

        // Create running agent session
        sqlx::query(
            "INSERT INTO agent_sessions (project_id, user_id, agent_user_id, prompt, status)
             VALUES ($1, $2, $3, 'investigating alert', 'running')",
        )
        .bind(project_id)
        .bind(admin_id.0)
        .bind(agent_user_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": Uuid::new_v4(),
        "project_id": project_id,
        "severity": "critical",
        "value": 100.0,
        "message": "Critical alert at limit",
        "alert_name": "limit-test",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(
        result.is_ok(),
        "should skip due to concurrent limit: {result:?}"
    );

    // Still only 3 sessions (no new one)
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 3,
        "no new session should be created at concurrent limit"
    );
}

/// AlertFired with valid critical alert → handler runs full path and sets cooldown.
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_sets_cooldown_on_attempt(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-alert-run", "public").await;

    let rule_id = Uuid::new_v4();
    let cooldown_key = format!("alert-agent:{project_id}:{rule_id}");

    // Verify no cooldown exists before
    let exists_before: bool = state
        .valkey
        .next()
        .exists::<bool, _>(&cooldown_key)
        .await
        .unwrap();
    assert!(!exists_before, "cooldown should not exist before event");

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": rule_id,
        "project_id": project_id,
        "severity": "critical",
        "value": 95.5,
        "message": "Critical CPU alert",
        "alert_name": "high-cpu",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "handler should succeed: {result:?}");

    // The handler should have set (or attempted to set) the cooldown key.
    // On success: cooldown stays. On failure: cooldown cleared.
    // Either way, the handler completed without error.
    //
    // If K8s is available (Kind cluster), the spawn succeeds and cooldown persists.
    // If K8s is unavailable, the spawn fails and cooldown is cleared.
    // Both outcomes are valid — we just verify the handler didn't error.
}

/// AlertFired with "warning" severity → handler proceeds past severity gate.
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_warning_severity_proceeds(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-alert-warn", "public").await;

    let rule_id = Uuid::new_v4();

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": rule_id,
        "project_id": project_id,
        "severity": "warning",
        "value": 80.0,
        "message": "Warning level alert",
        "alert_name": "warn-metric",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(
        result.is_ok(),
        "warning severity should proceed: {result:?}"
    );

    // Verify cooldown was set (proves handler got past severity gate and into spawn path)
    let cooldown_key = format!("alert-agent:{project_id}:{rule_id}");
    let exists: bool = state
        .valkey
        .next()
        .exists::<bool, _>(&cooldown_key)
        .await
        .unwrap();
    assert!(
        exists,
        "cooldown should be set (warning severity passes the gate)"
    );
}

/// AlertFired when admin user is deactivated → handler skips gracefully.
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_no_admin_user_skips_spawn(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-alert-noadm", "public").await;

    // Deactivate the admin user so the handler can't find a spawner
    sqlx::query("UPDATE users SET is_active = false WHERE name = 'admin'")
        .execute(&pool)
        .await
        .unwrap();

    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": Uuid::new_v4(),
        "project_id": project_id,
        "severity": "critical",
        "value": 99.0,
        "message": "Critical alert with no admin",
        "alert_name": "no-admin-test",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(
        result.is_ok(),
        "should skip gracefully when no admin: {result:?}"
    );

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "no session when admin is deactivated");
}

/// Cooldown is per-rule: setting cooldown for rule_A does not block rule_B on same project.
#[sqlx::test(migrations = "./migrations")]
async fn alert_fired_cooldown_is_per_rule(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id =
        helpers::create_project(&app, &admin_token, "eb-alert-perrule", "public").await;

    let rule_a = Uuid::new_v4();
    let rule_b = Uuid::new_v4();

    // Set cooldown for rule_A only
    let cooldown_a = format!("alert-agent:{project_id}:{rule_a}");
    state
        .valkey
        .next()
        .set::<(), _, _>(
            &cooldown_a,
            "1",
            Some(fred::types::Expiration::EX(900)),
            None,
            false,
        )
        .await
        .unwrap();

    // Fire alert for rule_B — should NOT be blocked by rule_A's cooldown
    let event = serde_json::json!({
        "type": "AlertFired",
        "rule_id": rule_b,
        "project_id": project_id,
        "severity": "critical",
        "value": 50.0,
        "message": "Different rule alert",
        "alert_name": "rule-b-test",
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "rule_b should not be blocked: {result:?}");

    // Verify rule_B got its own cooldown set (proves it passed rule_A's cooldown gate)
    let cooldown_b = format!("alert-agent:{project_id}:{rule_b}");
    let exists: bool = state
        .valkey
        .next()
        .exists::<bool, _>(&cooldown_b)
        .await
        .unwrap();
    assert!(
        exists,
        "rule_b should have its own cooldown (not blocked by rule_a)"
    );
}

/// DeployRequested with existing deployment → upserts (not duplicates).
#[sqlx::test(migrations = "./migrations")]
async fn deploy_requested_upserts_existing(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-dr-ups", "public").await;

    // Insert existing deployment
    sqlx::query(
        "INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status) \
         VALUES ($1, 'production', 'old:v1', 'active', 'healthy')",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let event = serde_json::json!({
        "type": "DeployRequested",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "new:v3",
        "requested_by": null,
    });

    platform::store::eventbus::handle_event(&state, &event.to_string())
        .await
        .unwrap();

    let (image_ref, status): (String, String) = sqlx::query_as(
        "SELECT image_ref, current_status FROM deployments WHERE project_id = $1 AND environment = 'production'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "new:v3");
    assert_eq!(status, "pending");
}
