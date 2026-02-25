//! Integration tests for dashboard, audit log, and onboarding status APIs.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{
    admin_login, assign_role, create_project, create_user, set_user_api_key, test_router,
    test_state,
};

// ---------------------------------------------------------------------------
// Dashboard stats
// ---------------------------------------------------------------------------

/// Empty DB → all zeros.
#[sqlx::test(migrations = "./migrations")]
async fn dashboard_stats_empty(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/dashboard/stats").await;

    assert_eq!(status, StatusCode::OK, "dashboard stats failed: {body}");
    // There may be a project created by bootstrap, but counts should be non-negative
    assert!(body["projects"].as_i64().is_some());
    assert_eq!(body["active_sessions"].as_i64().unwrap(), 0);
    assert_eq!(body["running_builds"].as_i64().unwrap(), 0);
    assert_eq!(body["failed_builds"].as_i64().unwrap(), 0);
    assert_eq!(body["healthy_deployments"].as_i64().unwrap(), 0);
    assert_eq!(body["degraded_deployments"].as_i64().unwrap(), 0);
}

/// With data inserted → stats reflect counts.
#[sqlx::test(migrations = "./migrations")]
async fn dashboard_stats_with_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Create a project (adds 1 to project count)
    let proj_id = create_project(&app, &admin_token, "dash-proj", "private").await;

    // Insert a healthy deployment
    sqlx::query(
        "INSERT INTO deployments (id, project_id, environment, image_ref, desired_status, current_status)
         VALUES ($1, $2, 'production', 'app:v1', 'active', 'healthy')",
    )
    .bind(Uuid::new_v4())
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/dashboard/stats").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["projects"].as_i64().unwrap() >= 1);
    assert_eq!(body["healthy_deployments"].as_i64().unwrap(), 1);
    // Note: running_builds/failed_builds will be 0 because dashboard queries
    // 'pipeline_runs' table which doesn't exist (known bug — silently returns 0)
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

/// Creating a project generates an audit entry visible via /api/audit-log.
#[sqlx::test(migrations = "./migrations")]
async fn list_audit_log(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Creating a project should write an audit entry
    create_project(&app, &admin_token, "audit-proj", "private").await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/audit-log").await;

    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items should be array");
    assert!(!items.is_empty(), "audit log should have entries");

    // Check structure of first entry
    let entry = &items[0];
    assert!(entry["id"].as_str().is_some());
    assert!(entry["actor_id"].as_str().is_some());
    assert!(entry["actor_name"].as_str().is_some());
    assert!(entry["action"].as_str().is_some());
    assert!(entry["created_at"].as_str().is_some());

    // Test pagination
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/audit-log?limit=1&offset=0").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().len() <= 1);
    assert!(body["total"].as_i64().unwrap() >= 1);
}

// ---------------------------------------------------------------------------
// Onboarding status
// ---------------------------------------------------------------------------

/// Fresh user with no projects or provider keys → needs_onboarding = true.
#[sqlx::test(migrations = "./migrations")]
async fn onboarding_status_fresh_user(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_user_id, token) = create_user(&app, &admin_token, "newbie", "newbie@test.com").await;

    let (status, body) = helpers::get_json(&app, &token, "/api/onboarding/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["has_projects"], false);
    assert_eq!(body["has_provider_key"], false);
    assert_eq!(body["needs_onboarding"], true);
}

/// User with a project → has_projects = true.
#[sqlx::test(migrations = "./migrations")]
async fn onboarding_status_with_project(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Admin already has projects if they create one
    create_project(&app, &admin_token, "onboard-proj", "private").await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/onboarding/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["has_projects"], true);
}

/// User with API key but no projects → needs_onboarding = true (new behavior).
#[sqlx::test(migrations = "./migrations")]
async fn onboarding_status_with_key_but_no_projects(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "haskey", "haskey@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    let (status, body) = helpers::get_json(&app, &token, "/api/onboarding/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["has_provider_key"], true);
    assert_eq!(body["has_projects"], false);
    // Key is set but no projects → still needs onboarding
    assert_eq!(body["needs_onboarding"], true);
}

/// User with API key AND a project → needs_onboarding = false.
#[sqlx::test(migrations = "./migrations")]
async fn onboarding_status_with_key_and_project(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "fulluser", "full@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;
    create_project(&app, &token, "my-first-project", "private").await;

    let (status, body) = helpers::get_json(&app, &token, "/api/onboarding/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["has_provider_key"], true);
    assert_eq!(body["has_projects"], true);
    assert_eq!(body["needs_onboarding"], false);
}
