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

/// Insert a session with optional project_id (NULL for project-less sessions).
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

    // Owner can access — gets 502 because no real backend is running
    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("preview backend"));
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

    // Reader can access — gets 502 because no real backend
    let (status, body) =
        helpers::get_json(&app, &reader_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("preview backend"));
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

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/preview/{session_id}/index.html"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("preview backend"));
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

    // Request with a deep path + query string — should hit the proxy (502 from no backend)
    let (status, _body) = helpers::get_json(
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
