// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Additional integration tests for `src/api/preview.rs` coverage gaps.
//!
//! Covers: deploy-preview auth, invalid service name, namespace validation,
//! project not found, project with no namespace, non-owner session access for
//! project-less sessions.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{create_project, create_user, test_router, test_state};

/// Insert a session row directly (bypasses K8s pod creation).
async fn insert_session(
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

/// Get admin user ID from auth/me.
async fn get_admin_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, body) = helpers::get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Preview proxy: non-owner access to private project session
// ---------------------------------------------------------------------------

/// Non-owner without project read cannot access preview of a running session.
#[sqlx::test(migrations = "./migrations")]
async fn proxy_non_owner_private_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let project_id = create_project(&app, &admin_token, "preview-priv", "private").await;
    let session_id = insert_session(
        &pool,
        Some(project_id),
        admin_id,
        "running",
        Some("test-ns"),
    )
    .await;

    // Create user with no access to the project
    let (_uid, user_token) =
        create_user(&app, &admin_token, "prev-noacc", "prevnoacc@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Non-owner cannot access preview of project-less session (no project_id).
#[sqlx::test(migrations = "./migrations")]
async fn proxy_non_owner_projectless_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_session(&pool, None, admin_id, "running", Some("test-ns")).await;

    let (_uid, user_token) =
        create_user(&app, &admin_token, "prev-noprj", "prevnoprj@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Session with invalid namespace format returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn proxy_invalid_namespace_format_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let project_id = create_project(&app, &admin_token, "preview-badns", "private").await;
    let session_id = insert_session(
        &pool,
        Some(project_id),
        admin_id,
        "running",
        Some("INVALID_NS!"),
    )
    .await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("invalid namespace")
    );
}

/// Owner can access their own running session preview (returns 502 since no backend).
#[sqlx::test(migrations = "./migrations")]
async fn proxy_owner_running_session_reaches_proxy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let project_id = create_project(&app, &admin_token, "preview-owner", "private").await;
    let session_id = insert_session(
        &pool,
        Some(project_id),
        admin_id,
        "running",
        Some("test-ns"),
    )
    .await;

    // This should reach the proxy layer and fail to connect (502 Bad Gateway)
    let status = helpers::get_status(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

/// Preview with subpath reaches proxy.
#[sqlx::test(migrations = "./migrations")]
async fn proxy_with_subpath_reaches_proxy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let project_id = create_project(&app, &admin_token, "preview-path", "private").await;
    let session_id = insert_session(
        &pool,
        Some(project_id),
        admin_id,
        "running",
        Some("test-ns"),
    )
    .await;

    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/preview/{session_id}/index.html"),
    )
    .await;
    // Should reach proxy and fail (502) since no real backend
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// Deploy preview proxy
// ---------------------------------------------------------------------------

/// Deploy preview with invalid service name returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_invalid_service_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "deploy-prev-bad", "public").await;

    // Service name with uppercase = invalid
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/INVALID_SVC/production"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("invalid service"));
}

/// Deploy preview for nonexistent project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_nonexistent_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{fake_id}/my-svc/production"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Deploy preview auth: non-member of private project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_private_project_non_member(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "dp-priv", "private").await;
    let (_uid, user_token) = create_user(&app, &admin_token, "dp-noacc", "dpnoacc@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/deploy-preview/{project_id}/my-svc/production"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Deploy preview with no auth returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_no_auth_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        "",
        &format!("/deploy-preview/{fake_id}/my-svc/production"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
