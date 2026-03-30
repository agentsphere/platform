//! Contract integration tests — verify JSON response shapes match what the UI expects.
//!
//! These tests don't duplicate business logic tests. They focus on:
//! - Correct field names (catches serde rename bugs)
//! - Correct field types (string, number, bool, null, array, object)
//! - `ListResponse` wrapper on list endpoints (`{items: [...], total: N}`)
//! - Nullable fields can actually be null
//!
//! Every assertion here corresponds to a field access in the UI code.

mod helpers;

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use helpers::{create_project, create_user, test_router, test_state};

// =========================================================================
// Helpers — reusable shape assertions
// =========================================================================

/// Assert a value is a non-empty UUID string.
fn assert_uuid(v: &Value, ctx: &str) {
    let s = v
        .as_str()
        .unwrap_or_else(|| panic!("{ctx}: expected string"));
    Uuid::parse_str(s).unwrap_or_else(|_| panic!("{ctx}: expected UUID, got {s}"));
}

/// Assert a value is an ISO 8601 timestamp string.
fn assert_timestamp(v: &Value, ctx: &str) {
    let s = v
        .as_str()
        .unwrap_or_else(|| panic!("{ctx}: expected string"));
    assert!(
        s.contains('T') || s.contains('-'),
        "{ctx}: expected timestamp, got {s}"
    );
}

/// Assert a value is a JSON number (i64 or f64).
fn assert_number(v: &Value, ctx: &str) {
    assert!(v.is_number(), "{ctx}: expected number, got {v}");
}

/// Assert `ListResponse` shape: {items: [...], total: N}
fn assert_list_response<'a>(body: &'a Value, ctx: &str) -> &'a Vec<Value> {
    assert!(body["items"].is_array(), "{ctx}: missing items array");
    assert_number(&body["total"], &format!("{ctx}.total"));
    body["items"].as_array().unwrap()
}

// =========================================================================
// P0: Auth endpoints
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_login_response(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({"name": "admin", "password": "testpassword"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    // LoginResponse: { token, expires_at, user: { id, name, email, ... } }
    assert!(body["token"].is_string(), "missing token");
    assert_timestamp(&body["expires_at"], "login.expires_at");

    let user = &body["user"];
    assert_uuid(&user["id"], "login.user.id");
    assert!(user["name"].is_string(), "missing user.name");
    assert!(user["email"].is_string(), "missing user.email");
    assert!(user["is_active"].is_boolean(), "missing user.is_active");
    assert_timestamp(&user["created_at"], "login.user.created_at");
    // display_name and user_type are nullable/present
    assert!(
        user["user_type"].is_string(),
        "missing user.user_type: {user}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_me_response(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/auth/me").await;

    assert_eq!(status, StatusCode::OK);

    // User: { id, name, email, display_name, user_type, is_active, created_at, updated_at }
    assert_uuid(&body["id"], "me.id");
    assert!(body["name"].is_string(), "missing name");
    assert!(body["email"].is_string(), "missing email");
    assert!(body["is_active"].is_boolean(), "missing is_active");
    assert!(body["user_type"].is_string(), "missing user_type");
    assert_timestamp(&body["created_at"], "me.created_at");
    assert_timestamp(&body["updated_at"], "me.updated_at");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_logout(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, _) =
        helpers::post_json(&app, &token, "/api/auth/logout", serde_json::json!({})).await;

    // Logout returns 200 with {ok: true} or 204
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "logout returned {status}"
    );
}

// =========================================================================
// P0: Dashboard endpoints
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_dashboard_stats(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/dashboard/stats").await;

    assert_eq!(status, StatusCode::OK);

    // DashboardStats: 6 numeric fields
    assert_number(&body["projects"], "stats.projects");
    assert_number(&body["active_sessions"], "stats.active_sessions");
    assert_number(&body["running_builds"], "stats.running_builds");
    assert_number(&body["failed_builds"], "stats.failed_builds");
    assert_number(&body["healthy_deployments"], "stats.healthy_deployments");
    assert_number(&body["degraded_deployments"], "stats.degraded_deployments");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_audit_log_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    // Create something to generate audit entries
    create_project(&app, &token, "audit-contract", "private").await;

    // Audit entries are written asynchronously — wait for them to land
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let (status, body) = helpers::get_json(&app, &token, "/api/audit-log?limit=10").await;

    assert_eq!(status, StatusCode::OK);

    let items = assert_list_response(&body, "audit-log");
    assert!(!items.is_empty(), "expected at least one audit entry");

    // AuditLogEntry shape
    let entry = &items[0];
    assert_uuid(&entry["id"], "audit.id");
    assert_uuid(&entry["actor_id"], "audit.actor_id");
    assert!(entry["actor_name"].is_string(), "missing actor_name");
    assert!(entry["action"].is_string(), "missing action");
    assert!(entry["resource"].is_string(), "missing resource");
    assert_timestamp(&entry["created_at"], "audit.created_at");
    // resource_id and detail can be null
    assert!(
        entry["resource_id"].is_string() || entry["resource_id"].is_null(),
        "resource_id should be string or null"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_onboarding_status(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/onboarding/status").await;

    assert_eq!(status, StatusCode::OK);

    // OnboardingStatus: { has_projects, has_provider_key, needs_onboarding }
    assert!(
        body["has_projects"].is_boolean(),
        "missing has_projects: {body}"
    );
    assert!(
        body["has_provider_key"].is_boolean(),
        "missing has_provider_key: {body}"
    );
    assert!(
        body["needs_onboarding"].is_boolean(),
        "missing needs_onboarding: {body}"
    );
}

// =========================================================================
// P0: Notification endpoints
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_notifications_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/notifications?limit=10").await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "notifications");
    // Even if empty, shape should be correct
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_notifications_unread_count(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/notifications/unread-count").await;

    assert_eq!(status, StatusCode::OK);
    assert_number(&body["count"], "unread_count.count");
}

// =========================================================================
// P0: Project CRUD
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_project_create(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/projects",
        serde_json::json!({"name": "contract-proj", "visibility": "private", "description": "A test project"}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);

    // Project response shape
    assert_uuid(&body["id"], "project.id");
    assert_eq!(body["name"], "contract-proj");
    assert!(body["visibility"].is_string(), "missing visibility");
    assert!(body["default_branch"].is_string(), "missing default_branch");
    assert_timestamp(&body["created_at"], "project.created_at");
    assert_timestamp(&body["updated_at"], "project.updated_at");
    assert_uuid(&body["owner_id"], "project.owner_id");
    // description and display_name can be null
    assert!(
        body["description"].is_string() || body["description"].is_null(),
        "description should be string or null"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_project_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    create_project(&app, &token, "list-proj", "private").await;

    let (status, body) = helpers::get_json(&app, &token, "/api/projects?limit=10").await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "projects");
    assert!(!items.is_empty(), "expected at least one project");

    // Each item should be a full Project
    let proj = &items[0];
    assert_uuid(&proj["id"], "project[0].id");
    assert!(proj["name"].is_string(), "missing project name");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_project_get(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let proj_id = create_project(&app, &token, "get-proj", "private").await;

    let (status, body) = helpers::get_json(&app, &token, &format!("/api/projects/{proj_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "project.id");
    assert_eq!(body["name"], "get-proj");
    assert!(body["visibility"].is_string(), "missing visibility");
    assert!(body["default_branch"].is_string(), "missing default_branch");
    assert_uuid(&body["owner_id"], "project.owner_id");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_project_update(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let proj_id = create_project(&app, &token, "update-proj", "private").await;

    let (status, body) = helpers::patch_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}"),
        serde_json::json!({"description": "Updated desc"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "project.id");
    assert_eq!(body["description"], "Updated desc");
}

// =========================================================================
// P0: Issues
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_issue_crud(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "issue-proj", "private").await;

    // Create issue
    let (status, body) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/issues"),
        serde_json::json!({"title": "Bug report", "body": "Something broken", "labels": ["bug"]}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_uuid(&body["id"], "issue.id");
    assert_uuid(&body["project_id"], "issue.project_id");
    assert_number(&body["number"], "issue.number");
    assert_eq!(body["title"], "Bug report");
    assert!(body["status"].is_string(), "missing issue status");
    assert_uuid(&body["author_id"], "issue.author_id");
    assert_timestamp(&body["created_at"], "issue.created_at");
    assert_timestamp(&body["updated_at"], "issue.updated_at");
    // labels should be an array
    assert!(body["labels"].is_array(), "labels should be array");

    let issue_num = body["number"].as_i64().unwrap();

    // List issues
    let (status, list_body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/issues?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&list_body, "issues");
    assert!(!items.is_empty());

    // Get single issue
    let (status, get_body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/issues/{issue_num}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(get_body["title"], "Bug report");

    // Create comment
    let (status, comment) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/issues/{issue_num}/comments"),
        serde_json::json!({"body": "A comment"}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_uuid(&comment["id"], "comment.id");
    assert!(comment["body"].is_string(), "missing comment body");
    assert_uuid(&comment["author_id"], "comment.author_id");
    assert_timestamp(&comment["created_at"], "comment.created_at");
    assert_timestamp(&comment["updated_at"], "comment.updated_at");

    // List comments
    let (status, comments) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/issues/{issue_num}/comments"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Comments endpoint returns ListResponse (items + total)
    let comment_items = assert_list_response(&comments, "comments");
    assert!(!comment_items.is_empty());
}

// =========================================================================
// P0: Merge Requests
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_merge_request_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "mr-proj", "private").await;

    // MR list should return ListResponse even if empty
    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/merge-requests?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "merge-requests");
}

// =========================================================================
// P0: Pipelines
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_pipeline_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "pipe-proj", "private").await;

    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/pipelines?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "pipelines");
}

// =========================================================================
// P0: Deployments
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_deployment_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "dep-proj", "private").await;

    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/deploy-releases?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "releases");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_deployment_with_data(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "dep-data-proj", "private").await;

    // Insert a deploy target + release
    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy, is_active)
         VALUES ($1, $2, 'staging', 'staging', 'rolling', true)",
    )
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();
    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, health)
         VALUES ($1, $2, $3, 'app:v1', 'rolling', 'progressing', 'healthy')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/deploy-releases?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "releases");
    assert!(!items.is_empty());

    // Release shape
    let rel = &items[0];
    assert_uuid(&rel["id"], "release.id");
    assert_uuid(&rel["project_id"], "release.project_id");
    assert_uuid(&rel["target_id"], "release.target_id");
    assert!(rel["image_ref"].is_string(), "missing image_ref");
    assert!(rel["strategy"].is_string(), "missing strategy");
    assert!(rel["phase"].is_string(), "missing phase");
    assert!(rel["health"].is_string(), "missing health");
    assert_timestamp(&rel["created_at"], "release.created_at");
}

// =========================================================================
// P0: Webhooks
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_webhook_crud(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "wh-proj", "private").await;

    // Create webhook
    let (status, body) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/webhooks"),
        serde_json::json!({"url": "https://example.com/hook", "events": ["push", "issue"]}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_uuid(&body["id"], "webhook.id");
    assert_uuid(&body["project_id"], "webhook.project_id");
    assert_eq!(body["url"], "https://example.com/hook");
    assert!(body["events"].is_array(), "missing events array");
    assert!(body["active"].is_boolean(), "missing active");
    assert_timestamp(&body["created_at"], "webhook.created_at");

    // List webhooks
    let (status, list_body) =
        helpers::get_json(&app, &token, &format!("/api/projects/{proj_id}/webhooks")).await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&list_body, "webhooks");
    assert!(!items.is_empty());
}

// =========================================================================
// P0: Secrets
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_secrets_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "sec-proj", "private").await;

    // Create a secret
    let (status, body) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({"name": "MY_SECRET", "value": "supersecret", "scope": "pipeline"}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["name"].is_string(), "missing secret name");
    assert!(body["scope"].is_string(), "missing scope");
    assert_timestamp(&body["created_at"], "secret.created_at");
    // Value should NOT be returned
    assert!(
        body["value"].is_null() || body.get("value").is_none(),
        "secret value should not be returned"
    );

    // List secrets
    let (status, list_body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/secrets?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&list_body, "secrets");
    assert!(!items.is_empty());
}

// =========================================================================
// P0: Project sessions
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_project_sessions_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "sess-proj", "private").await;

    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/sessions?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "sessions");
}

// =========================================================================
// P0: Admin — Users
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_admin_user_create(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/users",
        serde_json::json!({"name": "contract-user", "email": "cu@test.com", "password": "securepass123"}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_uuid(&body["id"], "user.id");
    assert_eq!(body["name"], "contract-user");
    assert_eq!(body["email"], "cu@test.com");
    assert!(body["is_active"].is_boolean(), "missing is_active");
    assert!(body["user_type"].is_string(), "missing user_type");
    assert_timestamp(&body["created_at"], "user.created_at");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_admin_user_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/users?limit=10").await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "users");
    assert!(!items.is_empty());

    let user = &items[0];
    assert_uuid(&user["id"], "user.id");
    assert!(user["name"].is_string(), "missing name");
    assert!(user["email"].is_string(), "missing email");
}

// =========================================================================
// P0: Admin — Roles
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_admin_roles(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/admin/roles").await;

    assert_eq!(status, StatusCode::OK);
    // Roles endpoint returns ListResponse
    let roles = body["items"]
        .as_array()
        .expect("roles should have items array");
    assert!(!roles.is_empty());

    // Role shape
    let role = &roles[0];
    assert_uuid(&role["id"], "role.id");
    assert!(role["name"].is_string(), "missing role name");
    assert!(role["is_system"].is_boolean(), "missing is_system");
}

// =========================================================================
// P0: Admin — Delegations
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_admin_delegation_shape(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (user_id, _) = create_user(&app, &token, "deleg-target", "deleg@test.com").await;

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/admin/delegations",
        serde_json::json!({
            "delegate_id": user_id.to_string(),
            "permission": "project:read",
            "reason": "contract test"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);

    // Delegation shape — UI uses permission_name (not permission)
    assert_uuid(&body["id"], "delegation.id");
    assert_uuid(&body["delegator_id"], "delegation.delegator_id");
    assert_uuid(&body["delegate_id"], "delegation.delegate_id");
    assert_uuid(&body["permission_id"], "delegation.permission_id");
    assert!(
        body["permission_name"].is_string(),
        "missing permission_name: {body}"
    );
    assert_timestamp(&body["created_at"], "delegation.created_at");
    // Optional fields
    assert!(
        body["project_id"].is_string() || body["project_id"].is_null(),
        "project_id should be string or null"
    );
    assert!(
        body["reason"].is_string() || body["reason"].is_null(),
        "reason should be string or null"
    );
}

// =========================================================================
// P0: Admin — Permissions
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_admin_permissions(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    // Get any role to list its permissions
    let (_, roles) = helpers::get_json(&app, &token, "/api/admin/roles").await;
    let admin_role = roles["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "admin")
        .unwrap();
    let role_id = admin_role["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/admin/roles/{role_id}/permissions"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let perms = body["items"]
        .as_array()
        .expect("permissions should have items array");
    assert!(!perms.is_empty());

    // Permission shape
    let perm = &perms[0];
    assert_uuid(&perm["id"], "permission.id");
    assert!(perm["name"].is_string(), "missing permission name");
}

// =========================================================================
// P0: API Tokens
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_api_tokens(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    // Create token
    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/tokens",
        serde_json::json!({"name": "my-token", "scopes": ["project:read"], "expires_in_days": 30}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "create token failed: {body}");

    // CreateTokenResponse: { id, name, token, scopes, expires_at, created_at }
    assert_uuid(&body["id"], "token.id");
    assert_eq!(body["name"], "my-token");
    assert!(body["token"].is_string(), "missing token value");
    assert!(
        body["token"].as_str().unwrap().starts_with("plat_"),
        "token should start with plat_"
    );
    assert!(body["scopes"].is_array(), "missing scopes");
    assert_timestamp(&body["created_at"], "token.created_at");

    // List tokens
    let (status, list_body) = helpers::get_json(&app, &token, "/api/tokens").await;

    assert_eq!(status, StatusCode::OK);
    let tokens = list_body["items"]
        .as_array()
        .expect("tokens should have items array");
    assert!(!tokens.is_empty());

    // Token list shape (no plaintext token in list)
    let t = &tokens[0];
    assert_uuid(&t["id"], "token.id");
    assert!(t["name"].is_string(), "missing token name");
    assert!(t["scopes"].is_array(), "missing scopes");
}

// =========================================================================
// P0: Preview Deployments
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_preview_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "preview-proj", "private").await;

    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{proj_id}/targets?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "targets");
}

// =========================================================================
// P0: Observe — Alerts
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_alert_rules_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::get_json(&app, &token, "/api/observe/alerts?limit=10").await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "alert-rules");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_alert_rule_create(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "high-error-rate",
            "query": "metric:http_errors",
            "condition": "gt",
            "threshold": 100.0,
            "window_seconds": 300,
            "channels": ["webhook"],
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "create alert rule failed: {body}"
    );

    // AlertRule shape — check serde renames
    assert_uuid(&body["id"], "alert_rule.id");
    assert_eq!(body["name"], "high-error-rate");
    assert!(body["query"].is_string(), "missing query");
    assert!(body["condition"].is_string(), "missing condition");
    // threshold can be null
    assert!(
        body["threshold"].is_number() || body["threshold"].is_null(),
        "threshold should be number or null"
    );
    // UI accesses window_seconds (serde rename from for_seconds)
    assert_number(&body["window_seconds"], "alert_rule.window_seconds");
    // UI accesses channels (serde rename from notify_channels)
    assert!(body["channels"].is_array(), "missing channels");
    assert!(body["enabled"].is_boolean(), "missing enabled");
    assert_timestamp(&body["created_at"], "alert_rule.created_at");
}

// =========================================================================
// P0: Observe — Logs & Traces (empty queries)
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_observe_logs_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) =
        helpers::get_json(&app, &token, "/api/observe/logs?limit=10&time_range=1h").await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "logs");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_observe_traces_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();

    let (status, body) =
        helpers::get_json(&app, &token, "/api/observe/traces?limit=10&time_range=1h").await;

    assert_eq!(status, StatusCode::OK);
    assert_list_response(&body, "traces");
}

// =========================================================================
// P0: Git Browser
// =========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn contract_branches_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let token = admin_token.clone();
    let proj_id = create_project(&app, &token, "git-proj", "private").await;

    // Branches for a fresh project (may be empty or have default branch)
    let (status, body) =
        helpers::get_json(&app, &token, &format!("/api/projects/{proj_id}/branches")).await;

    // Could be 200 with empty array or 404 if no repo initialized
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "branches returned {status}: {body}"
    );
    if status == StatusCode::OK {
        assert!(body.is_array(), "branches should be array");
    }
}

// =========================================================================
// MCP Contract Tests — verify response shapes for endpoints called by
// MCP servers (mcp/servers/*.js) that aren't already covered above.
// =========================================================================

// -- platform-admin MCP: user management via /api/users/* --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_admin_get_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (user_id, _) = create_user(&app, &admin_token, "mcp-get-user", "mcp-get@test.com").await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "user.id");
    assert!(body["name"].is_string(), "missing name");
    assert!(body["email"].is_string(), "missing email");
    assert!(body["is_active"].is_boolean(), "missing is_active");
    assert!(body["user_type"].is_string(), "missing user_type");
    assert_timestamp(&body["created_at"], "user.created_at");
    assert_timestamp(&body["updated_at"], "user.updated_at");
    // display_name nullable
    assert!(
        body["display_name"].is_string() || body["display_name"].is_null(),
        "display_name should be string or null"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_admin_update_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (user_id, _) = create_user(&app, &admin_token, "mcp-upd-user", "mcp-upd@test.com").await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/users/{user_id}"),
        serde_json::json!({"display_name": "Updated Name"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "user.id");
    assert_eq!(body["display_name"], "Updated Name");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_admin_deactivate_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (user_id, _) =
        create_user(&app, &admin_token, "mcp-deact-user", "mcp-deact@test.com").await;

    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
}

// -- platform-admin MCP: role management --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_admin_create_role(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        serde_json::json!({"name": "mcp-test-role", "description": "A test role"}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "create role failed: {body}");
    assert_uuid(&body["id"], "role.id");
    assert_eq!(body["name"], "mcp-test-role");
    assert!(body["is_system"].is_boolean(), "missing is_system");
    assert_timestamp(&body["created_at"], "role.created_at");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_admin_assign_role(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (user_id, _) = create_user(&app, &admin_token, "mcp-role-user", "mcp-role@test.com").await;

    // Get a role to assign
    let (_, roles_body) = helpers::get_json(&app, &admin_token, "/api/admin/roles").await;
    let viewer_role = roles_body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "viewer")
        .expect("viewer role should exist");
    let role_id = viewer_role["id"].as_str().unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{user_id}/roles"),
        serde_json::json!({"role_id": role_id}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "assign role failed: {body}");
    assert!(body["ok"].is_boolean(), "missing ok field");
}

// -- platform-admin MCP: delegation management --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_admin_list_delegations(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/delegations").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body["items"].is_array(),
        "delegations should have items array"
    );
}

// -- platform-issues MCP: issue update --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_issue_update(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-iss-proj", "private").await;

    // Create an issue first
    let (_, issue) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/issues"),
        serde_json::json!({"title": "Original title", "labels": ["bug"]}),
    )
    .await;
    let num = issue["number"].as_i64().unwrap();

    // Update it
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/issues/{num}"),
        serde_json::json!({"title": "Updated title", "status": "closed", "labels": ["bug", "fixed"]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "issue.id");
    assert_eq!(body["title"], "Updated title");
    assert_eq!(body["status"], "closed");
    assert!(body["labels"].is_array(), "labels should be array");
    assert_eq!(body["labels"].as_array().unwrap().len(), 2);
    assert_number(&body["number"], "issue.number");
    assert_uuid(&body["author_id"], "issue.author_id");
    assert_timestamp(&body["updated_at"], "issue.updated_at");
}

// -- platform-issues MCP: merge request CRUD --
// Note: MR creation requires real git branches, so we insert MRs directly
// into the DB (like issue_mr_integration.rs does) and test the GET/PATCH/comment shapes.

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_mr_get_shape(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-mr-get", "private").await;

    // Get admin user ID
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Insert MR directly (bypasses branch validation)
    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, body, status)
         VALUES ($1, $2, 1, $3, 'feat/test', 'main', 'Test MR', 'MR body text', 'open')",
    )
    .bind(mr_id)
    .bind(proj_id)
    .bind(admin_id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(proj_id)
        .execute(&pool)
        .await
        .unwrap();

    // GET MR
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/merge-requests/1"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "mr.id");
    assert_uuid(&body["project_id"], "mr.project_id");
    assert_number(&body["number"], "mr.number");
    assert_eq!(body["title"], "Test MR");
    assert_eq!(body["source_branch"], "feat/test");
    assert_eq!(body["target_branch"], "main");
    assert!(body["status"].is_string(), "missing mr status");
    assert_uuid(&body["author_id"], "mr.author_id");
    assert_timestamp(&body["created_at"], "mr.created_at");
    assert_timestamp(&body["updated_at"], "mr.updated_at");
    // merged_by/merged_at should be null for open MR
    assert!(body["merged_by"].is_null(), "merged_by should be null");
    assert!(body["merged_at"].is_null(), "merged_at should be null");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_mr_update(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-mr-upd", "private").await;

    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status)
         VALUES ($1, $2, 1, $3, 'feat/upd', 'main', 'Original MR', 'open')",
    )
    .bind(mr_id)
    .bind(proj_id)
    .bind(admin_id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(proj_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/merge-requests/1"),
        serde_json::json!({"title": "Updated MR"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Updated MR");
    assert_uuid(&body["id"], "mr.id");
    assert_timestamp(&body["updated_at"], "mr.updated_at");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_mr_comment(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-mr-cmt", "private").await;

    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status)
         VALUES ($1, $2, 1, $3, 'feat/cmt', 'main', 'MR for comments', 'open')",
    )
    .bind(mr_id)
    .bind(proj_id)
    .bind(admin_id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(proj_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/merge-requests/1/comments"),
        serde_json::json!({"body": "LGTM!"}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "create MR comment failed: {body}"
    );
    assert_uuid(&body["id"], "comment.id");
    assert!(body["body"].is_string(), "missing comment body");
    assert_uuid(&body["author_id"], "comment.author_id");
    assert_timestamp(&body["created_at"], "comment.created_at");
    assert_timestamp(&body["updated_at"], "comment.updated_at");
}

// -- platform-deploy MCP: deployment detail + history --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_deployment_get(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-dep-get", "private").await;

    // Insert deploy target + release
    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy, is_active)
         VALUES ($1, $2, 'prod', 'production', 'rolling', true)",
    )
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();
    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, health)
         VALUES ($1, $2, $3, 'myapp:v2', 'rolling', 'progressing', 'healthy')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/deploy-releases/{release_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "release.id");
    assert_uuid(&body["project_id"], "release.project_id");
    assert_uuid(&body["target_id"], "release.target_id");
    assert_eq!(body["image_ref"], "myapp:v2");
    assert!(body["strategy"].is_string(), "missing strategy");
    assert!(body["phase"].is_string(), "missing phase");
    assert!(body["health"].is_string(), "missing health");
    assert_timestamp(&body["created_at"], "release.created_at");
    assert_timestamp(&body["updated_at"], "release.updated_at");
    // Nullable fields
    assert!(
        body["deployed_by"].is_string() || body["deployed_by"].is_null(),
        "deployed_by should be string or null"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_deployment_history(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-dep-hist", "private").await;

    // Insert deploy target + release + history
    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy, is_active)
         VALUES ($1, $2, 'staging', 'staging', 'rolling', true)",
    )
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();
    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, health)
         VALUES ($1, $2, $3, 'app:v1', 'rolling', 'progressing', 'healthy')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO release_history (id, release_id, target_id, action, phase, image_ref)
         VALUES ($1, $2, $3, 'created', 'pending', 'app:v1')",
    )
    .bind(Uuid::new_v4())
    .bind(release_id)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/deploy-releases/{release_id}/history?limit=10"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "release-history");
    assert!(!items.is_empty());

    let entry = &items[0];
    assert_uuid(&entry["id"], "history.id");
    assert_uuid(&entry["release_id"], "history.release_id");
    assert_uuid(&entry["target_id"], "history.target_id");
    assert!(entry["image_ref"].is_string(), "missing image_ref");
    assert!(entry["action"].is_string(), "missing action");
    assert!(entry["phase"].is_string(), "missing phase");
    assert_timestamp(&entry["created_at"], "history.created_at");
}

// -- platform-observe MCP: metrics + metric names --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_observe_metrics(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        "/api/observe/metrics?name=http_requests&time_range=1h",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Returns Vec<MetricSeries> (array)
    assert!(body.is_array(), "metrics should be array");
}

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_observe_metric_names(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/observe/metrics/names").await;

    assert_eq!(status, StatusCode::OK);
    // Returns Vec<MetricNameResponse> (array)
    assert!(body.is_array(), "metric names should be array");
}

// -- platform-observe MCP: alert detail --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_observe_alert_get(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    // Create an alert rule first
    let (_, alert) = helpers::post_json(
        &app,
        &admin_token,
        "/api/observe/alerts",
        serde_json::json!({
            "name": "mcp-alert-get",
            "query": "metric:http_errors",
            "condition": "gt",
            "threshold": 50.0,
            "window_seconds": 60,
            "channels": ["webhook"],
        }),
    )
    .await;
    let alert_id = alert["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/observe/alerts/{alert_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "alert.id");
    assert_eq!(body["name"], "mcp-alert-get");
    assert!(body["query"].is_string(), "missing query");
    assert!(body["condition"].is_string(), "missing condition");
    assert!(body["enabled"].is_boolean(), "missing enabled");
    assert_number(&body["window_seconds"], "alert.window_seconds");
    assert!(body["channels"].is_array(), "missing channels");
    assert_timestamp(&body["created_at"], "alert.created_at");
}

// -- platform-core MCP: session detail --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_session_detail(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-sess-proj", "private").await;

    // Insert a session directly
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'test prompt', 'running', 'anthropic')",
    )
    .bind(session_id)
    .bind(proj_id)
    .bind(admin_id.0)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/sessions/{session_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_uuid(&body["id"], "session.id");
    assert!(body["status"].is_string(), "missing status");
    assert!(body["prompt"].is_string(), "missing prompt");
    assert!(body["provider"].is_string(), "missing provider");
    assert_uuid(&body["user_id"], "session.user_id");
    assert_timestamp(&body["created_at"], "session.created_at");
    // messages should be an array (from SessionDetailResponse flatten)
    assert!(body["messages"].is_array(), "missing messages array");
}

// =========================================================================
// MCP Contract Tests — additional coverage for endpoints called by
// MCP servers that weren't covered by the tests above.
// =========================================================================

// -- platform-core MCP: list_projects shape (MCP calls GET /api/projects) --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_core_list_projects(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    // Create a project so we have data
    create_project(&app, &admin_token, "mcp-list-proj", "private").await;

    // MCP server: apiGet("/api/projects", { query: { limit, offset, search } })
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/projects?limit=50&offset=0").await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "mcp_core_list_projects");
    assert!(!items.is_empty(), "expected at least one project");

    // MCP accesses: data.items[].id, data.items[].name, data.total
    let proj = &items[0];
    assert_uuid(&proj["id"], "project.id");
    assert!(proj["name"].is_string(), "missing project name");
    assert!(proj["visibility"].is_string(), "missing visibility");
    assert!(proj["default_branch"].is_string(), "missing default_branch");
    assert_uuid(&proj["owner_id"], "project.owner_id");
    assert_timestamp(&proj["created_at"], "project.created_at");
}

// -- platform-core MCP: create_project shape (MCP calls POST /api/projects) --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_core_create_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    // MCP server: apiPost("/api/projects", { body: { name, display_name, description, visibility } })
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({
            "name": "mcp-created-proj",
            "description": "Created via MCP",
            "visibility": "private"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_uuid(&body["id"], "project.id");
    assert_eq!(body["name"], "mcp-created-proj");
    assert!(body["visibility"].is_string(), "missing visibility");
    assert!(body["default_branch"].is_string(), "missing default_branch");
    assert_uuid(&body["owner_id"], "project.owner_id");
    assert_timestamp(&body["created_at"], "project.created_at");
    assert_timestamp(&body["updated_at"], "project.updated_at");
}

// -- platform-pipeline MCP: list_pipelines shape --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_pipeline_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-pipe-list", "private").await;

    // Insert a pipeline directly to verify shape when data exists
    let pipeline_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, status)
         VALUES ($1, $2, 'api', 'main', 'pending')",
    )
    .bind(pipeline_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    // MCP server: apiGet(`/api/projects/${p}/pipelines`, { query: { status, trigger, git_ref, limit, offset } })
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/pipelines?limit=50"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "mcp_pipeline_list");
    assert!(!items.is_empty(), "expected at least one pipeline");

    let pipe = &items[0];
    assert_uuid(&pipe["id"], "pipeline.id");
    assert_uuid(&pipe["project_id"], "pipeline.project_id");
    assert!(pipe["trigger"].is_string(), "missing trigger");
    assert!(pipe["git_ref"].is_string(), "missing git_ref");
    assert!(pipe["status"].is_string(), "missing status");
    assert_timestamp(&pipe["created_at"], "pipeline.created_at");
    // Nullable fields
    assert!(
        pipe["commit_sha"].is_string() || pipe["commit_sha"].is_null(),
        "commit_sha should be string or null"
    );
    assert!(
        pipe["triggered_by"].is_string() || pipe["triggered_by"].is_null(),
        "triggered_by should be string or null"
    );
    assert!(
        pipe["started_at"].is_string() || pipe["started_at"].is_null(),
        "started_at should be string or null"
    );
    assert!(
        pipe["finished_at"].is_string() || pipe["finished_at"].is_null(),
        "finished_at should be string or null"
    );
}

// -- platform-pipeline MCP: trigger_pipeline shape --
// The MCP calls POST /api/projects/{id}/pipelines { git_ref }.
// Triggering requires a valid repo with .platform.yaml — we insert a pipeline
// directly to test the GET detail shape instead (the POST would fail without
// a valid repo). The response shape is PipelineDetailResponse for GET.

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_pipeline_trigger(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-pipe-trig", "private").await;

    // Insert a pipeline + step directly to test GET detail (same shape as trigger response)
    let pipeline_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, commit_sha, status)
         VALUES ($1, $2, 'api', 'main', 'abc123def456', 'pending')",
    )
    .bind(pipeline_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    let step_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipeline_steps (id, pipeline_id, project_id, step_order, name, image, status, gate)
         VALUES ($1, $2, $3, 0, 'build', 'alpine:latest', 'pending', false)",
    )
    .bind(step_id)
    .bind(pipeline_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    // GET pipeline detail — same shape MCP receives after trigger
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/pipelines/{pipeline_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // PipelineDetailResponse = PipelineResponse (flattened) + steps
    assert_uuid(&body["id"], "pipeline.id");
    assert_uuid(&body["project_id"], "pipeline.project_id");
    assert!(body["trigger"].is_string(), "missing trigger");
    assert!(body["git_ref"].is_string(), "missing git_ref");
    assert!(body["status"].is_string(), "missing status");
    assert_timestamp(&body["created_at"], "pipeline.created_at");
    // steps array
    assert!(body["steps"].is_array(), "missing steps array");
    let steps = body["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "expected at least one step");

    let step = &steps[0];
    assert_uuid(&step["id"], "step.id");
    assert_number(&step["step_order"], "step.step_order");
    assert!(step["name"].is_string(), "missing step name");
    assert!(step["image"].is_string(), "missing step image");
    assert!(step["status"].is_string(), "missing step status");
    assert!(step["gate"].is_boolean(), "missing step gate");
    assert!(step["depends_on"].is_array(), "missing step depends_on");
    assert_timestamp(&step["created_at"], "step.created_at");
}

// -- platform-pipeline MCP: get_step_logs shape --
// MCP calls GET /api/projects/{id}/pipelines/{pid}/steps/{sid}/logs.
// Returns text/plain. We test a completed step with no log_ref (returns "No logs available").

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_pipeline_get_logs(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-pipe-logs", "private").await;

    let pipeline_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, status)
         VALUES ($1, $2, 'api', 'main', 'success')",
    )
    .bind(pipeline_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    let step_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipeline_steps (id, pipeline_id, project_id, step_order, name, image, status, gate)
         VALUES ($1, $2, $3, 0, 'build', 'alpine:latest', 'success', false)",
    )
    .bind(step_id)
    .bind(pipeline_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    // GET step logs — returns text/plain
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!(
            "/api/projects/{proj_id}/pipelines/{pipeline_id}/steps/{step_id}/logs"
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/plain"),
        "step logs should return text/plain, got: {content_type}"
    );
}

// -- platform-deploy MCP: list_targets shape --

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_deploy_list_targets(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-tgt-list", "private").await;

    // Insert a deploy target
    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy, is_active)
         VALUES ($1, $2, 'staging', 'staging', 'rolling', true)",
    )
    .bind(target_id)
    .bind(proj_id)
    .execute(&pool)
    .await
    .unwrap();

    // MCP server: apiGet(`/api/projects/${p}/targets`, { query: { limit, offset } })
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/targets?limit=50"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "mcp_deploy_list_targets");
    assert!(!items.is_empty(), "expected at least one target");

    let target = &items[0];
    assert_uuid(&target["id"], "target.id");
    assert_uuid(&target["project_id"], "target.project_id");
    assert!(target["name"].is_string(), "missing target name");
    assert!(target["environment"].is_string(), "missing environment");
    assert!(
        target["default_strategy"].is_string(),
        "missing default_strategy"
    );
    assert!(target["is_active"].is_boolean(), "missing is_active");
    assert_timestamp(&target["created_at"], "target.created_at");
}

// -- platform-deploy MCP: staging_status shape --
// staging_status requires an ops repo with real git branches. Without an ops repo
// the endpoint returns 404 (ApiError::NotFound). We verify this 404 contract.

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_deploy_staging_status(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-staging", "private").await;

    // Without an ops repo, staging-status returns 404
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/staging-status"),
    )
    .await;

    // MCP deploy server calls: apiGet(`/api/projects/${p}/staging-status`)
    // Expected shape when ops repo exists: { diverged: bool, staging_image, prod_image, staging_sha, prod_sha }
    // Without ops repo: 404 — verify the endpoint is wired and returns proper error
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "staging-status without ops repo should be 404: {body}"
    );
}

// -- platform-core MCP: spawn_agent shape --
// MCP calls POST /api/projects/{id}/sessions/{sid}/spawn { prompt }

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_core_spawn_agent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-spawn-proj", "private").await;

    // Get admin user ID
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Insert a parent session
    let parent_session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, spawn_depth)
         VALUES ($1, $2, $3, 'parent task', 'running', 'anthropic', 0)",
    )
    .bind(parent_session_id)
    .bind(proj_id)
    .bind(admin_id.0)
    .execute(&pool)
    .await
    .unwrap();

    // MCP server: apiPost(`/api/projects/${PROJECT_ID}/sessions/${SESSION_ID}/spawn`, { body: { prompt } })
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/sessions/{parent_session_id}/spawn"),
        serde_json::json!({"prompt": "Fix the failing tests"}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "spawn failed: {body}");
    // SessionResponse shape
    assert_uuid(&body["id"], "session.id");
    assert!(body["status"].is_string(), "missing status");
    assert!(body["prompt"].is_string(), "missing prompt");
    assert_eq!(body["prompt"], "Fix the failing tests");
    assert!(body["provider"].is_string(), "missing provider");
    assert_uuid(&body["user_id"], "session.user_id");
    assert_timestamp(&body["created_at"], "session.created_at");
    // project_id should match
    assert_uuid(&body["project_id"], "session.project_id");
}

// -- MCP flags: list flags shape --
// No dedicated MCP flags server yet, but gate.js lists toggle_flag as UPDATE.
// The platform has /api/projects/{id}/flags endpoint. Verify the contract.

#[sqlx::test(migrations = "./migrations")]
async fn contract_mcp_flags_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let proj_id = create_project(&app, &admin_token, "mcp-flags-proj", "private").await;

    // Create a flag directly
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO feature_flags (project_id, key, flag_type, default_value, enabled, created_by)
         VALUES ($1, 'dark-mode', 'boolean', 'true', true, $2)",
    )
    .bind(proj_id)
    .bind(admin_id.0)
    .execute(&pool)
    .await
    .unwrap();

    // GET /api/projects/{id}/flags
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/flags?limit=50"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = assert_list_response(&body, "mcp_flags_list");
    assert!(!items.is_empty(), "expected at least one flag");

    let flag = &items[0];
    assert_uuid(&flag["id"], "flag.id");
    assert!(flag["key"].is_string(), "missing flag key");
    assert!(flag["flag_type"].is_string(), "missing flag_type");
    assert!(flag["enabled"].is_boolean(), "missing enabled");
    assert_timestamp(&flag["created_at"], "flag.created_at");
    assert_timestamp(&flag["updated_at"], "flag.updated_at");
    // Nullable fields
    assert!(
        flag["environment"].is_string() || flag["environment"].is_null(),
        "environment should be string or null"
    );
    assert!(
        flag["description"].is_string() || flag["description"].is_null(),
        "description should be string or null"
    );
}
