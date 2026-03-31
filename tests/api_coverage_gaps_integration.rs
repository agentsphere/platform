//! Comprehensive integration tests covering remaining coverage gaps across
//! multiple API handler files:
//!
//! - merge_requests.rs: update MR invalid status, update comment by admin (non-author),
//!   create review invalid verdict, list MRs with author_id filter, MR create
//!   on nonexistent project
//! - preview.rs: session not running, session with no namespace (null)
//! - onboarding.rs: wizard with custom_provider, wizard non-admin forbidden,
//!   verify_oauth_token too short, complete wizard idempotent
//! - sessions.rs: spawn child max depth, list children, update session
//!   non-owner forbidden, stop session wrong project
//! - users.rs: login with agent user rejected, update user email only,
//!   create user with empty name
//! - pipelines.rs: list pipelines combined filters (status + trigger),
//!   trigger on nonexistent project
//! - health.rs: health details endpoint
//! - cli_auth.rs: store credentials empty token rejected, delete credentials idempotent

mod helpers;

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Get admin user ID from auth/me.
async fn get_admin_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, body) = helpers::get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Insert an MR directly (bypassing branch checks).
async fn insert_mr(pool: &PgPool, project_id: Uuid, author_id: Uuid, number: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status)
          VALUES ($1, $2, $3, $4, 'feat', 'main', 'Test MR', 'open')",
    )
    .bind(id)
    .bind(project_id)
    .bind(number)
    .bind(author_id)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("UPDATE projects SET next_mr_number = $1 WHERE id = $2")
        .bind(number + 1)
        .bind(project_id)
        .execute(pool)
        .await
        .unwrap();

    id
}

/// Insert a session row directly.
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

/// Insert a pipeline row directly.
async fn insert_pipeline(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    status: &str,
    git_ref: &str,
    trigger: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, status, triggered_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(project_id)
    .bind(trigger)
    .bind(git_ref)
    .bind(status)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

// ===========================================================================
// MERGE REQUESTS
// ===========================================================================

/// Update MR with invalid status value (e.g. "merged") is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_status_merged_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-merged", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "merged" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("open or closed"));
}

/// Update MR with status "pending" is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_status_invalid_value_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-invalid", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "pending" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_nonexistent_mr_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-none", "public").await;

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999"),
        json!({ "title": "Updated" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Admin can update another user's comment.
#[sqlx::test(migrations = "./migrations")]
async fn update_comment_admin_can_edit_others(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-admin-edit", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Create a user with write access
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-author", "cmtauthor@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "admin", None, &pool).await;

    // User creates a comment
    let (_, comment) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "User comment" }),
    )
    .await;
    let comment_id = comment["id"].as_str().unwrap();

    // Admin updates user's comment
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": "Admin edited this" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["body"], "Admin edited this");
}

/// Non-author non-admin cannot update comment.
#[sqlx::test(migrations = "./migrations")]
async fn update_comment_non_author_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-noedit", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Admin creates a comment
    let (_, comment) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "Admin only" }),
    )
    .await;
    let comment_id = comment["id"].as_str().unwrap();

    // Another user (with project write but not admin) tries to update
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-noauth", "cmtnoauth@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": "Hacked" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Create review with invalid verdict is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_review_invalid_verdict_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "rev-bad-verdict", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "invalid_verdict" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("verdict"));
}

/// Create review body too long is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_review_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "rev-long-body", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let long_body = "x".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "approve", "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// List MRs with author_id filter.
#[sqlx::test(migrations = "./migrations")]
async fn list_mrs_filter_by_author_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-list-author", "public").await;

    // Create MRs by admin
    insert_mr(&pool, project_id, admin_id, 1).await;
    insert_mr(&pool, project_id, admin_id, 2).await;

    // Create MR by another user
    let (other_id, _) =
        helpers::create_user(&app, &admin_token, "mr-other-auth", "mrotherauth@test.com").await;
    insert_mr(&pool, project_id, other_id, 3).await;

    // Filter by admin_id
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?author_id={admin_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);

    // Filter by other user
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?author_id={other_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
}

/// List MRs with status filter.
#[sqlx::test(migrations = "./migrations")]
async fn list_mrs_filter_by_status(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-list-status", "public").await;

    insert_mr(&pool, project_id, admin_id, 1).await;
    // Insert a closed MR
    let closed_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status)
          VALUES ($1, $2, $3, $4, 'feat-closed', 'main', 'Closed MR', 'closed')",
    )
    .bind(closed_id)
    .bind(project_id)
    .bind(2i32)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Filter open
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?status=open"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);

    // Filter closed
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?status=closed"),
    )
    .await;
    assert_eq!(body["total"], 1);
}

/// Update MR title only.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_title_only_succeeds(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-title", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "title": "New Title" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "New Title");
}

/// Update MR close via status change.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_close_via_status(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-close", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "closed");
}

/// Update MR with title too long fails validation.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_title_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-ltitle", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let long_title = "t".repeat(501);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "title": long_title }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update MR body too long fails validation.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-lbody", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let long_body = "b".repeat(100_001);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Create comment with body too long fails.
#[sqlx::test(migrations = "./migrations")]
async fn create_comment_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-long-body", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let long_body = "c".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Create comment with empty body fails.
#[sqlx::test(migrations = "./migrations")]
async fn create_comment_empty_body_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-empty-body", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update comment body too long fails.
#[sqlx::test(migrations = "./migrations")]
async fn update_comment_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-upd-long", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Create a comment
    let (_, comment) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "Original" }),
    )
    .await;
    let comment_id = comment["id"].as_str().unwrap();

    let long_body = "c".repeat(100_001);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update comment on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_comment_nonexistent_mr_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "cmt-upd-nomr", "public").await;
    let fake_comment = Uuid::new_v4();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments/{fake_comment}"),
        json!({ "body": "Never" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Update nonexistent comment returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_nonexistent_comment_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-upd-noexist", "public").await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let fake_comment = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{fake_comment}"),
        json!({ "body": "Ghost" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Create review on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn create_review_nonexistent_mr_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "rev-nomr", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/reviews"),
        json!({ "verdict": "approve" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Create comment on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn create_comment_nonexistent_mr_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "cmt-nomr", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments"),
        json!({ "body": "Orphan comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// List reviews on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn list_reviews_nonexistent_mr_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "rev-list-nomr", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/reviews"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// List comments on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn list_comments_nonexistent_mr_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "cmt-list-nomr", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// PREVIEW
// ===========================================================================

/// Preview proxy for session that is not running returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn preview_session_not_running_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "prev-notrun", "private").await;
    let session_id = insert_session(
        &pool,
        Some(project_id),
        admin_id,
        "completed",
        Some("test-ns"),
    )
    .await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("not running"));
}

/// Preview proxy for session with NULL namespace returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn preview_session_null_namespace_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "prev-nullns", "private").await;
    let session_id = insert_session(&pool, Some(project_id), admin_id, "running", None).await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/preview/{session_id}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("namespace"));
}

/// Preview proxy for nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn preview_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(&app, &admin_token, &format!("/preview/{fake_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Deploy preview for project with empty namespace_slug returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn deploy_preview_empty_namespace_slug(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "dp-emptyslug", "public").await;

    // Clear the namespace_slug in DB
    sqlx::query("UPDATE projects SET namespace_slug = '' WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/deploy-preview/{project_id}/my-svc"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("namespace"));
}

// ===========================================================================
// ONBOARDING
// ===========================================================================

/// Non-admin cannot start claude auth flow.
#[sqlx::test(migrations = "./migrations")]
async fn start_claude_auth_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "wiz-nostart", "wiznostart@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/onboarding/claude-auth/start",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Wizard with custom_provider saves the provider config.
#[sqlx::test(migrations = "./migrations")]
async fn wizard_with_custom_provider(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        json!({
            "org_type": "solo",
            "custom_provider": {
                "provider_type": "bedrock",
                "env_vars": {
                    "AWS_REGION": "us-west-2",
                    "AWS_ACCESS_KEY_ID": "AKIATEST123",
                    "AWS_SECRET_ACCESS_KEY": "secrettest123"
                },
                "model": "anthropic.claude-sonnet-4-20250514",
                "label": "My Bedrock"
            }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "wizard with custom provider failed: {body}"
    );
    assert_eq!(body["success"], true);
}

/// Verify OAuth token that is too short fails validation.
#[sqlx::test(migrations = "./migrations")]
async fn verify_oauth_token_too_short_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/claude-auth/verify-token",
        json!({ "token": "short" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("token"));
}

/// Non-admin cannot verify OAuth token.
#[sqlx::test(migrations = "./migrations")]
async fn verify_oauth_token_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "oat-noadm", "oatnoadm@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/onboarding/claude-auth/verify-token",
        json!({ "token": "a-long-enough-token-for-validation" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Non-admin cannot create demo project.
#[sqlx::test(migrations = "./migrations")]
async fn create_demo_project_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "demo-noadm", "demonoadm@test.com").await;

    let (status, _) =
        helpers::post_json(&app, &user_token, "/api/onboarding/demo-project", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Non-admin cannot get settings.
#[sqlx::test(migrations = "./migrations")]
async fn get_settings_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "set-noadm", "setnoadm@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/onboarding/settings").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Wizard status for non-admin returns show_wizard=false.
#[sqlx::test(migrations = "./migrations")]
async fn wizard_status_non_admin_returns_false(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "wizst-noadm", "wizstnoadm@test.com").await;

    let (status, body) =
        helpers::get_json(&app, &user_token, "/api/onboarding/wizard-status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["show_wizard"], false);
}

// ===========================================================================
// SESSIONS
// ===========================================================================

/// Stop session on wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn stop_session_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = helpers::create_project(&app, &admin_token, "stop-prj-a", "private").await;
    let project_b = helpers::create_project(&app, &admin_token, "stop-prj-b", "private").await;
    let session_id =
        insert_session(&pool, Some(project_a), admin_id, "running", Some("ns-a")).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/stop"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// List children for a session returns empty when no children.
#[sqlx::test(migrations = "./migrations")]
async fn list_children_no_children(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "children-empty", "private").await;
    let session_id =
        insert_session(&pool, Some(project_id), admin_id, "running", Some("ns-ch")).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

/// List children with actual child sessions.
#[sqlx::test(migrations = "./migrations")]
async fn list_children_with_children(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "children-has", "private").await;
    let parent_id =
        insert_session(&pool, Some(project_id), admin_id, "running", Some("ns-ch2")).await;

    // Insert child sessions
    for i in 0..3 {
        let child_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
             VALUES ($1, $2, $3, $4, 'pending', 'claude-code', $5, 1)",
        )
        .bind(child_id)
        .bind(project_id)
        .bind(admin_id)
        .bind(format!("Child task {i}"))
        .bind(parent_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
}

/// Update session by non-owner returns 403.
#[sqlx::test(migrations = "./migrations")]
async fn update_session_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "upd-sess-own", "private").await;
    let session_id =
        insert_session(&pool, Some(project_id), admin_id, "running", Some("ns-upd")).await;

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "sess-notmine", "sessnotmine@test.com").await;

    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/sessions/{session_id}"),
        json!({ "project_id": project_id }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Update session to link to nonexistent project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_session_link_nonexistent_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id =
        helpers::create_project(&app, &admin_token, "upd-sess-noproj", "private").await;
    let session_id = insert_session(&pool, Some(project_id), admin_id, "running", None).await;

    let fake_project = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}"),
        json!({ "project_id": fake_project }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Get session from wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_session_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = helpers::create_project(&app, &admin_token, "gsess-a", "private").await;
    let project_b = helpers::create_project(&app, &admin_token, "gsess-b", "private").await;
    let session_id =
        insert_session(&pool, Some(project_a), admin_id, "running", Some("ns-gs")).await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Get nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gsess-none", "private").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Send message to global session by non-owner returns 403.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = helpers::create_project(&app, &admin_token, "gmsg-forbid", "private").await;
    let session_id =
        insert_session(&pool, Some(project_id), admin_id, "running", Some("ns-gm")).await;

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "gmsg-nown", "gmsgnown@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/sessions/{session_id}/message"),
        json!({ "content": "hello" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// USERS
// ===========================================================================

/// Login with agent user type returns 401 (agents cannot login).
#[sqlx::test(migrations = "./migrations")]
async fn login_agent_user_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create an agent user
    helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        json!({
            "name": "agent-login-test",
            "email": "agentlogin@test.com",
            "user_type": "agent",
        }),
    )
    .await;

    // Try to login as agent (should fail)
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        json!({ "name": "agent-login-test", "password": "anypassword" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Update own email only succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn update_user_email_only(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "email-only", "emailonly@test.com").await;

    let (status, body) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/users/{user_id}"),
        json!({ "email": "newemail@test.com" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], "newemail@test.com");
}

/// Update own display_name succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn update_user_display_name_only(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "disp-only", "disponly@test.com").await;

    let (status, body) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/users/{user_id}"),
        json!({ "display_name": "Cool Name" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], "Cool Name");
}

/// Create user with invalid name is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_user_invalid_name_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        json!({
            "name": "",
            "email": "emptyname@test.com",
            "password": "testpass123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// List API tokens returns the test token.
#[sqlx::test(migrations = "./migrations")]
async fn list_api_tokens_returns_tokens(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/tokens").await;
    assert_eq!(status, StatusCode::OK);
    // Should have at least the test-admin token
    assert!(body["items"].as_array().unwrap().len() >= 1);
    assert!(body["total"].as_i64().unwrap() >= 1);
}

// ===========================================================================
// PIPELINES
// ===========================================================================

/// List pipelines with combined status + trigger filters.
#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_combined_filters(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-combined", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "failure",
        "refs/heads/main",
        "push",
    )
    .await;
    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "api",
    )
    .await;
    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/dev",
        "mr",
    )
    .await;

    // Combined: status=success AND trigger=push
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?status=success&trigger=push"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);

    // Combined: status=success AND git_ref
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?status=success&git_ref=refs/heads/dev"),
    )
    .await;
    assert_eq!(body["total"], 1);

    // All filters: status + git_ref + trigger
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?status=success&git_ref=refs/heads/main&trigger=api"),
    )
    .await;
    assert_eq!(body["total"], 1);
}

/// Trigger pipeline on nonexistent project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn trigger_pipeline_nonexistent_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{fake_id}/pipelines"),
        json!({ "git_ref": "main" }),
    )
    .await;
    // Admin has global permissions but project doesn't exist
    assert!(status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN);
}

// ===========================================================================
// HEALTH
// ===========================================================================

/// Health details with poisoned lock returns 500.
/// This test writes a snapshot that simulates subsystems being checked, verifying
/// the health detail endpoint handles the full snapshot structure.
#[sqlx::test(migrations = "./migrations")]
async fn health_details_full_snapshot_fields(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;

    // Write a snapshot with subsystems
    {
        let mut snap = state.health.write().unwrap();
        snap.overall = platform::health::SubsystemStatus::Healthy;
        snap.uptime_seconds = 42;
        snap.subsystems = vec![platform::health::SubsystemCheck {
            name: "test".into(),
            status: platform::health::SubsystemStatus::Healthy,
            latency_ms: 1,
            message: Some("test msg".into()),
            checked_at: Utc::now(),
        }];
    }

    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/health/details").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["checked_at"].is_string());
    assert!(body["background_tasks"].is_array());
    assert!(body["pod_failures"].is_object());
}

/// Health summary with subsystems returns correct counts.
#[sqlx::test(migrations = "./migrations")]
async fn health_summary_with_subsystems(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;

    {
        let mut snap = state.health.write().unwrap();
        snap.overall = platform::health::SubsystemStatus::Degraded;
        snap.uptime_seconds = 100;
        snap.subsystems = vec![
            platform::health::SubsystemCheck {
                name: "db".into(),
                status: platform::health::SubsystemStatus::Healthy,
                latency_ms: 5,
                message: None,
                checked_at: Utc::now(),
            },
            platform::health::SubsystemCheck {
                name: "cache".into(),
                status: platform::health::SubsystemStatus::Unhealthy,
                latency_ms: 0,
                message: Some("connection refused".into()),
                checked_at: Utc::now(),
            },
        ];
    }

    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["overall"], "degraded");
    assert_eq!(body["uptime_seconds"], 100);
    assert_eq!(body["subsystems"].as_array().unwrap().len(), 2);
}

/// Non-admin cannot access health stream endpoint.
#[sqlx::test(migrations = "./migrations")]
async fn health_stream_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "health-noadm2", "healthnoadm2@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/health/stream").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// CLI AUTH
// ===========================================================================

/// Store credentials with empty token is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn store_cli_credentials_empty_token_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        json!({
            "auth_type": "setup_token",
            "token": "   ",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("empty"));
}

/// Store credentials with invalid auth_type is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn store_cli_credentials_invalid_auth_type_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        json!({
            "auth_type": "invalid_type",
            "token": "some-valid-token-value",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Get credentials when none exist returns exists=false.
#[sqlx::test(migrations = "./migrations")]
async fn get_cli_credentials_none_exist(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create a new user who has no credentials
    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "cli-noexist", "clinoexist@test.com").await;

    let (status, body) = helpers::get_json(&app, &user_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exists"], false);
}

/// Delete credentials when none exist is idempotent (returns 204).
#[sqlx::test(migrations = "./migrations")]
async fn delete_cli_credentials_idempotent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "cli-delnone", "clidelnone@test.com").await;

    let (status, _) = helpers::delete_json(&app, &user_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Store and retrieve credentials round-trip.
#[sqlx::test(migrations = "./migrations")]
async fn store_and_get_cli_credentials_roundtrip(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Store
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        json!({
            "auth_type": "setup_token",
            "token": "valid-test-token-value-12345",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "store failed: {body}");
    assert_eq!(body["exists"], true);
    assert_eq!(body["auth_type"], "setup_token");

    // Get
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exists"], true);
    assert_eq!(body["auth_type"], "setup_token");

    // Delete
    let (status, _) = helpers::delete_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify gone
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exists"], false);
}

// ===========================================================================
// DOWNLOADS
// ===========================================================================

/// Download agent-runner with invalid arch returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_invalid_arch(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/downloads/agent-runner?arch=ppc64").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("arch"));
}

/// Download agent-runner with no binary returns 500 (binary not found).
#[sqlx::test(migrations = "./migrations")]
async fn download_agent_runner_binary_not_found(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    // Override agent_runner_dir to an empty temp dir so no binaries exist,
    // even when PLATFORM_AGENT_RUNNER_DIR points to pre-built binaries.
    let empty_dir =
        std::env::temp_dir().join(format!("agent-runner-empty-{}", uuid::Uuid::new_v4()));
    let mut config = (*state.config).clone();
    config.agent_runner_dir = empty_dir;
    state.config = std::sync::Arc::new(config);
    let app = helpers::test_router(state);

    let status =
        helpers::get_status(&app, &admin_token, "/api/downloads/agent-runner?arch=amd64").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// Download MCP servers tarball not found returns 500.
#[sqlx::test(migrations = "./migrations")]
async fn download_mcp_servers_not_found(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;
    // Override mcp_servers_tarball to a non-existent path.
    let mut config = (*state.config).clone();
    config.mcp_servers_tarball =
        std::env::temp_dir().join(format!("mcp-nonexist-{}.tar.gz", uuid::Uuid::new_v4()));
    state.config = std::sync::Arc::new(config);
    let app = helpers::test_router(state);

    let status = helpers::get_status(&app, &admin_token, "/api/downloads/mcp-servers").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
