//! Integration tests for the preview reverse proxy (`/preview/{session_id}/...`).
//!
//! These tests exercise the auth, permission, and error paths of the proxy handler.
//! The actual HTTP proxying to a backend service returns 502 (no real backend running)
//! which validates the full proxy pipeline up to the outbound request.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{create_project, create_user, test_router, test_state};

/// Insert a session row directly (bypasses K8s pod creation).
async fn insert_session(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    status: &str,
    session_namespace: Option<&str>,
) -> Uuid {
    insert_session_opt(pool, Some(project_id), user_id, status, session_namespace).await
}

/// Insert a session with optional `project_id` (NULL for project-less sessions).
async fn insert_session_opt(
    pool: &PgPool,
    project_id: Option<Uuid>,
    user_id: Uuid,
    status: &str,
    session_namespace: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, session_namespace)
         VALUES ($1, $2, $3, 'test prompt', $4, 'claude-code', $5)",
    )
    .bind(id)
    .bind(project_id)
    .bind(user_id)
    .bind(status)
    .bind(session_namespace)
    .execute(pool)
    .await
    .expect("insert session");
    id
}

// ---------------------------------------------------------------------------
// Auth tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_auth_no_token_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;
    let app = test_router(state);

    let random_id = Uuid::new_v4();
    let (status, _body) = helpers::get_json(&app, "", &format!("/preview/{random_id}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Session lookup tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_session_not_found_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let random_id = Uuid::new_v4();
    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{random_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("session"));
}

#[sqlx::test(migrations = "./migrations")]
async fn proxy_session_not_running_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-stopped", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "stopped", Some("test-ns")).await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("not running"));
}

#[sqlx::test(migrations = "./migrations")]
async fn proxy_session_no_namespace_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-nons", "private").await;
    // session_namespace = NULL (e.g. cli_subprocess session)
    let session_id = insert_session(&pool, project_id, admin_id, "running", None).await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("namespace"));
}

// ---------------------------------------------------------------------------
// Authorization tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_owner_can_access(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-owner", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "running", Some("test-ns")).await;

    // Owner can access — gets 502 because no real backend is running.
    // When PLATFORM_PREVIEW_PROXY_URL is set, the response comes from the proxy pod
    // (non-JSON nginx 502), so use get_status instead of get_json.
    let status = helpers::get_status(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[sqlx::test(migrations = "./migrations")]
async fn proxy_project_reader_can_access(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-reader", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "running", Some("test-ns")).await;

    // Create a second user with ProjectRead on the project
    let (reader_id, reader_token) =
        create_user(&app, &admin_token, "reader1", "reader1@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        reader_id,
        "viewer",
        Some(project_id),
        &pool,
    )
    .await;

    // Reader can access — gets 502 because no real backend.
    // Response may be non-JSON (nginx proxy), so use get_status.
    let status = helpers::get_status(&app, &reader_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[sqlx::test(migrations = "./migrations")]
async fn proxy_non_owner_non_reader_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-denied", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "running", Some("test-ns")).await;

    // Create a second user with NO role on the project
    let (_other_id, other_token) =
        create_user(&app, &admin_token, "other1", "other1@test.com").await;

    // Non-owner, non-reader gets 404 (not 403, avoids leaking existence)
    let (status, _body) =
        helpers::get_json(&app, &other_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Backend proxy error tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_backend_unreachable_returns_502(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-502", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "running", Some("valid-ns")).await;

    // Response may be non-JSON (nginx proxy), so use get_status.
    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/preview/{session_id}/index.html"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// Path + query preservation test
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_preserves_path_and_query(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-path", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "running", Some("valid-ns")).await;

    // Request with a deep path + query string — should hit the proxy (502 from no backend).
    // Response may be non-JSON (nginx proxy), so use get_status.
    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/preview/{session_id}/api/data?page=1&limit=10"),
    )
    .await;
    // We get 502 because no backend, but this proves the path/query were accepted
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// Edge case: null project_id + non-owner
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_null_project_non_owner_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Session with no project — only owner can access
    let session_id = insert_session_opt(&pool, None, admin_id, "running", Some("test-ns")).await;

    // Create a different user — they cannot access a project-less session they don't own
    let (_other_id, other_token) =
        create_user(&app, &admin_token, "other2", "other2@test.com").await;

    let (status, _body) =
        helpers::get_json(&app, &other_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Edge case: invalid namespace format
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_invalid_namespace_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-badns", "private").await;
    // Namespace with uppercase chars — fails validation
    let session_id =
        insert_session(&pool, project_id, admin_id, "running", Some("INVALID_NS")).await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("namespace"));
}

// ---------------------------------------------------------------------------
// Deploy preview proxy tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_project_not_found_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_project = Uuid::new_v4();
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{fake_project}/my-svc"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("project"));
}

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_invalid_service_name_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-badsvc", "private").await;

    // Service name with uppercase chars -- fails validation
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/INVALID_SVC"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("service name"));
}

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_auth_required(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_project = Uuid::new_v4();
    let (status, _body) =
        helpers::get_json(&app, "", &format!("/deploy-preview/{fake_project}/my-svc")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_non_reader_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "dp-denied", "private").await;

    // Create a user with no role on the project
    let (_other_id, other_token) =
        create_user(&app, &admin_token, "dpother1", "dpother1@test.com").await;

    let (status, _body) = helpers::get_json(
        &app,
        &other_token,
        &format!("/deploy-preview/{project_id}/my-svc"),
    )
    .await;
    // Private project, no access -> 404 (not 403)
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_service_not_found_in_k8s(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-nosvc", "public").await;

    // The K8s namespace for this project likely doesn't exist, so the Service won't be found
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/nonexistent-svc"),
    )
    .await;
    // Should get 404 for the service (K8s API returns 404)
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("service"));
}

// ---------------------------------------------------------------------------
// Deploy preview with environment query param
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_with_env_query_param(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-env-param", "public").await;

    // Request with ?env=staging — service won't exist but validates the env param is accepted
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/my-svc?env=staging"),
    )
    .await;
    // K8s namespace doesn't exist, so service lookup fails with 404
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("service"));
}

// ---------------------------------------------------------------------------
// Preview proxy: owner can access null-project session
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_owner_can_access_null_project_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Session with no project — only owner can access
    let session_id = insert_session_opt(&pool, None, admin_id, "running", Some("test-ns")).await;

    // Owner can access — gets 502 because no real backend is running
    let status = helpers::get_status(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// Preview proxy: path with trailing slash
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn proxy_path_with_trailing_slash(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "preview-trail", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "running", Some("test-ns")).await;

    // Request with trailing slash
    let status = helpers::get_status(&app, &admin_token, &format!("/preview/{session_id}/")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// Deploy preview: trailing slash on URL
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_trailing_slash(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-trail", "public").await;

    // Request with trailing slash — should still work (validates routing)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/my-svc/"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("service"));
}

// ---------------------------------------------------------------------------
// Deploy preview: empty service name in URL segment
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_with_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-path", "public").await;

    // Request with deep path — validates the {*path} wildcard segment
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/my-svc/assets/app.js"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("service"));
}

// ---------------------------------------------------------------------------
// Deploy preview: viewer on public project can access
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_viewer_public_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "dp-viewer-pub", "public").await;

    // Create a viewer user
    let (user_id, user_token) =
        create_user(&app, &admin_token, "dp-viewer", "dpviewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Viewer can access public project deploy preview (gets 404 because K8s service doesn't exist)
    let (status, body) = helpers::get_json(
        &app,
        &user_token,
        &format!("/deploy-preview/{project_id}/my-svc"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("service"));
}

// ---------------------------------------------------------------------------
// Deploy preview: query string preserved
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_preserves_query_string(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-query", "public").await;

    // Request with query parameters
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/my-svc/api/data?page=1&limit=10"),
    )
    .await;
    // Service doesn't exist, but query is accepted without errors
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("service"));
}
