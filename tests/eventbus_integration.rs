mod helpers;

use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Eventbus Integration Tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn handle_event_invalid_json(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let result = platform::store::eventbus::handle_event(&state, "not valid json").await;
    assert!(result.is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn handle_event_unknown_type(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let json = r#"{"type":"UnknownEvent","project_id":"00000000-0000-0000-0000-000000000000"}"#;
    let result = platform::store::eventbus::handle_event(&state, json).await;
    assert!(result.is_err());
}

/// ImageBuilt with no existing deployment → creates default deployment.
#[sqlx::test(migrations = "./migrations")]
async fn image_built_no_deployment_creates_default(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let admin_token = helpers::admin_login(&app).await;
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
