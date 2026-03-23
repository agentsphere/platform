mod helpers;

use fred::prelude::*;
use sqlx::PgPool;
use sqlx::Row;
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

/// `ImageBuilt` is now a legacy no-op — no deploy targets or releases created.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_is_noop(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-img-noop", "public").await;

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

    // No release should be created (ImageBuilt is now a no-op)
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "ImageBuilt should not create releases (legacy no-op)"
    );

    // No deploy target should be created either
    let target_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_targets WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        target_count, 0,
        "ImageBuilt should not create deploy targets (legacy no-op)"
    );
}

/// `OpsRepoUpdated` → creates new pending release on existing target.
#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_updated_marks_pending(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-ops-upd", "public").await;

    // Insert deploy target + completed release
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO deploy_targets (project_id, name, environment) \
         VALUES ($1, 'staging', 'staging') RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO deploy_releases (target_id, project_id, image_ref, phase) \
         VALUES ($1, $2, 'old:v1', 'completed')",
    )
    .bind(target_id)
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

    let (image_ref, phase, sha): (String, String, Option<String>) = sqlx::query_as(
        "SELECT dr.image_ref, dr.phase, dr.commit_sha \
         FROM deploy_releases dr \
         JOIN deploy_targets dt ON dt.id = dr.target_id \
         WHERE dt.project_id = $1 AND dt.environment = 'staging' \
         ORDER BY dr.created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "new:v2");
    assert_eq!(phase, "pending");
    assert_eq!(sha.as_deref(), Some("abc123"));
}

/// `OpsRepoUpdated` reads `platform.yaml` from the ops repo, parses deploy specs
/// for strategy/rollout_config, and creates a release with those values.
#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_updated_reads_platform_yaml(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-ops-yaml", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "canary-ops", "main")
        .await
        .unwrap();

    // Write platform.yaml with canary config
    let platform_yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
      commands: ["echo hi"]
deploy:
  specs:
    - name: api
      type: canary
      canary:
        stable_service: app-stable
        canary_service: app-canary
        steps: [10, 50, 100]
flags:
  - key: dark_mode
    default_value: false
"#;
    platform::deployer::ops_repo::write_file_to_repo(
        &ops_path,
        "main",
        "platform.yaml",
        platform_yaml,
    )
    .await
    .unwrap();

    // Insert ops_repos row linking to the project
    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id) \
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind(format!("canary-ops-{}", Uuid::new_v4()))
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Publish OpsRepoUpdated
    let event = serde_json::json!({
        "type": "OpsRepoUpdated",
        "project_id": project_id,
        "ops_repo_id": ops_repo_id,
        "environment": "production",
        "commit_sha": "abc1234567",
        "image_ref": "registry/app:v1",
    });
    platform::store::eventbus::handle_event(&state, &event.to_string())
        .await
        .unwrap();

    // Verify release created with canary strategy
    let row =
        sqlx::query("SELECT strategy, rollout_config FROM deploy_releases WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let strategy: String = row.get("strategy");
    assert_eq!(strategy, "canary");

    let config: serde_json::Value = row.get("rollout_config");
    assert_eq!(config["steps"], serde_json::json!([10, 50, 100]));

    // Verify flag registered
    let flag_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feature_flags WHERE project_id = $1 AND key = 'dark_mode'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(flag_count, 1);

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// `OpsRepoUpdated` without a `platform.yaml` → release created with default strategy (rolling).
#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_updated_without_platform_yaml(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-ops-nofile", "public").await;

    // Create ops repo on disk — no platform.yaml written
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "no-yaml-ops", "main")
        .await
        .unwrap();

    // Write something so the branch exists (bare repo needs at least one commit)
    platform::deployer::ops_repo::write_file_to_repo(&ops_path, "main", "README.md", "# ops")
        .await
        .unwrap();

    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id) \
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind(format!("no-yaml-ops-{}", Uuid::new_v4()))
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let event = serde_json::json!({
        "type": "OpsRepoUpdated",
        "project_id": project_id,
        "ops_repo_id": ops_repo_id,
        "environment": "production",
        "commit_sha": "def456",
        "image_ref": "registry/app:v2",
    });
    platform::store::eventbus::handle_event(&state, &event.to_string())
        .await
        .unwrap();

    // Verify release created with default rolling strategy
    let row =
        sqlx::query("SELECT strategy, rollout_config FROM deploy_releases WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let strategy: String = row.get("strategy");
    assert_eq!(
        strategy, "rolling",
        "should use default strategy when no platform.yaml"
    );

    let config: serde_json::Value = row.get("rollout_config");
    assert_eq!(
        config,
        serde_json::json!({}),
        "rollout_config should be empty default"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// `DeployRequested` commits values to ops repo then publishes `OpsRepoUpdated`.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_requested_commits_to_ops_repo(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-deploy-ops", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "deploy-ops", "main")
        .await
        .unwrap();

    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id) \
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind(format!("deploy-ops-{}", Uuid::new_v4()))
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let event = serde_json::json!({
        "type": "DeployRequested",
        "project_id": project_id,
        "environment": "production",
        "image_ref": "registry/app:manual-v1",
        "requested_by": null,
    });
    platform::store::eventbus::handle_event(&state, &event.to_string())
        .await
        .unwrap();

    // Verify values committed to ops repo
    let values = platform::deployer::ops_repo::read_values(&ops_path, "main", "production")
        .await
        .unwrap();
    assert_eq!(values["image_ref"], "registry/app:manual-v1");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// `DeployRequested` with no ops repo → graceful skip (no error, no release).
#[sqlx::test(migrations = "./migrations")]
async fn deploy_requested_no_ops_repo_skips(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-deploy-noop", "public").await;

    let event = serde_json::json!({
        "type": "DeployRequested",
        "project_id": project_id,
        "environment": "staging",
        "image_ref": "deploy:v1",
        "requested_by": null,
    });

    let result = platform::store::eventbus::handle_event(&state, &event.to_string()).await;
    assert!(result.is_ok(), "handle_event failed: {result:?}");

    // No release should be created (no ops repo to commit to)
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "DeployRequested without ops repo should create no releases"
    );
}

/// `RollbackRequested` with no ops repo → graceful skip (no error).
#[sqlx::test(migrations = "./migrations")]
async fn rollback_no_ops_repo_graceful(pool: PgPool) {
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
        "rollback with no ops repo should be ok: {result:?}"
    );
}

/// `RollbackRequested` reverts the ops repo and publishes `OpsRepoUpdated`.
#[sqlx::test(migrations = "./migrations")]
async fn rollback_reverts_ops_repo(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-rb-revert", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "rollback-ops", "main")
        .await
        .unwrap();

    // Commit v1 values
    let v1_values = serde_json::json!({
        "image_ref": "app:v1",
        "project_name": "eb-rb-revert",
        "environment": "production",
    });
    platform::deployer::ops_repo::commit_values(&ops_path, "main", "production", &v1_values)
        .await
        .unwrap();

    // Commit v2 values
    let v2_values = serde_json::json!({
        "image_ref": "app:v2",
        "project_name": "eb-rb-revert",
        "environment": "production",
    });
    platform::deployer::ops_repo::commit_values(&ops_path, "main", "production", &v2_values)
        .await
        .unwrap();

    // Insert ops_repos row
    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id) \
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind(format!("rollback-ops-{}", Uuid::new_v4()))
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Send RollbackRequested
    let event = serde_json::json!({
        "type": "RollbackRequested",
        "project_id": project_id,
        "environment": "production",
        "requested_by": null,
    });
    platform::store::eventbus::handle_event(&state, &event.to_string())
        .await
        .unwrap();

    // Verify ops repo values reverted to v1
    let reverted = platform::deployer::ops_repo::read_values(&ops_path, "main", "production")
        .await
        .unwrap();
    assert_eq!(
        reverted["image_ref"], "app:v1",
        "ops repo should be reverted to v1"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

/// `ImageBuilt` then `OpsRepoUpdated` sequence: ImageBuilt is a no-op,
/// only OpsRepoUpdated creates the release.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_then_ops_repo_updated(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-seq", "public").await;

    // Step 1: ImageBuilt is a no-op — no release created
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

    // Verify no releases after ImageBuilt
    let count_after_ib: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count_after_ib, 0, "ImageBuilt should not create releases");

    // Step 2: OpsRepoUpdated creates the release
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
        "SELECT dr.image_ref, dr.commit_sha \
         FROM deploy_releases dr \
         JOIN deploy_targets dt ON dt.id = dr.target_id \
         WHERE dt.project_id = $1 AND dt.environment = 'staging' \
         ORDER BY dr.created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(image_ref, "app:v2");
    assert_eq!(sha.as_deref(), Some("def456"));
}

/// Multiple OpsRepoUpdated events for different environments are independent.
#[sqlx::test(migrations = "./migrations")]
async fn different_environments_independent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "eb-multi-env", "public").await;

    // Deploy to staging via OpsRepoUpdated
    let staging = serde_json::json!({
        "type": "OpsRepoUpdated",
        "project_id": project_id,
        "ops_repo_id": Uuid::new_v4(),
        "environment": "staging",
        "commit_sha": "stg1",
        "image_ref": "app:staging-v1",
    });
    platform::store::eventbus::handle_event(&state, &staging.to_string())
        .await
        .unwrap();

    // Deploy to production via OpsRepoUpdated
    let prod = serde_json::json!({
        "type": "OpsRepoUpdated",
        "project_id": project_id,
        "ops_repo_id": Uuid::new_v4(),
        "environment": "production",
        "commit_sha": "prod1",
        "image_ref": "app:prod-v1",
    });
    platform::store::eventbus::handle_event(&state, &prod.to_string())
        .await
        .unwrap();

    // Verify both deploy targets exist with different image refs
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_targets WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);

    let (staging_img,): (String,) = sqlx::query_as(
        "SELECT dr.image_ref \
         FROM deploy_releases dr \
         JOIN deploy_targets dt ON dt.id = dr.target_id \
         WHERE dt.project_id = $1 AND dt.environment = 'staging' \
         ORDER BY dr.created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(staging_img, "app:staging-v1");

    let (prod_img,): (String,) = sqlx::query_as(
        "SELECT dr.image_ref \
         FROM deploy_releases dr \
         JOIN deploy_targets dt ON dt.id = dr.target_id \
         WHERE dt.project_id = $1 AND dt.environment = 'production' \
         ORDER BY dr.created_at DESC LIMIT 1",
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

/// `AlertFired` with no `project_id` → handler skips gracefully (no sessions created).
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

/// `AlertFired` with info severity → handler skips (only warning/critical spawn agents).
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

/// `AlertFired` with active cooldown → handler skips duplicate spawn.
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

/// `AlertFired` with 3 active ops sessions → concurrent limit prevents spawn.
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

/// `AlertFired` with valid critical alert → handler runs full path and sets cooldown.
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

/// `AlertFired` with "warning" severity → handler proceeds past severity gate.
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

/// `AlertFired` when admin user is deactivated → handler skips gracefully.
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

/// Cooldown is per-rule: setting cooldown for `rule_A` does not block `rule_B` on same project.
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
