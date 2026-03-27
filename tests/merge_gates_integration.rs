//! Integration tests for merge gate enforcement and auto-merge endpoints.
//!
//! T2: `enforce_merge_gates()` — branch protection rules block/allow merges
//! T3: `enable_auto_merge` / `disable_auto_merge` endpoints

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// T2: enforce_merge_gates() — rejection tests (no git repo needed)
// ---------------------------------------------------------------------------

/// Merge blocked when merge_method doesn't match protection rule.
#[sqlx::test(migrations = "./migrations")]
async fn merge_blocked_wrong_method(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-method", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Protection rule: only squash allowed
    helpers::insert_branch_protection(&pool, project_id, "main", 0, &["squash"], &[], false, false)
        .await;

    // Create MR targeting main
    helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Attempt merge with default method ("merge") — should be rejected
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject wrong merge method: {body}"
    );
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.to_lowercase().contains("method") || err.to_lowercase().contains("merge"),
        "error should mention merge method, got: {err}"
    );
}

/// Merge blocked when required approvals are not met.
#[sqlx::test(migrations = "./migrations")]
async fn merge_blocked_insufficient_approvals(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-approvals", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Protection rule: require 2 approvals
    helpers::insert_branch_protection(&pool, project_id, "main", 2, &["merge"], &[], false, false)
        .await;

    // Create MR with 0 reviews
    helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject insufficient approvals: {body}"
    );
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.to_lowercase().contains("approv"),
        "error should mention approvals, got: {err}"
    );
}

/// Merge blocked when CI check is required but no pipeline exists.
#[sqlx::test(migrations = "./migrations")]
async fn merge_blocked_no_ci_pipeline(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-ci-none", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Protection rule: require CI checks
    helpers::insert_branch_protection(
        &pool,
        project_id,
        "main",
        0,
        &["merge"],
        &["ci"],
        false,
        false,
    )
    .await;

    // Create MR — no pipeline exists
    helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject missing CI: {body}"
    );
}

/// Merge blocked when CI pipeline exists but failed.
#[sqlx::test(migrations = "./migrations")]
async fn merge_blocked_ci_failed(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-ci-fail", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Protection rule with CI requirement
    helpers::insert_branch_protection(
        &pool,
        project_id,
        "main",
        0,
        &["merge"],
        &["ci"],
        false,
        false,
    )
    .await;

    // Create MR + failed pipeline
    helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;
    helpers::insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "failure",
        "refs/heads/feat",
        "push",
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject failed CI: {body}"
    );
}

/// Admin bypass disabled: admin still blocked by gates.
#[sqlx::test(migrations = "./migrations")]
async fn merge_admin_no_bypass(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-no-bypass", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Protection rule: require 1 approval, NO admin bypass
    helpers::insert_branch_protection(&pool, project_id, "main", 1, &["merge"], &[], false, false)
        .await;

    // MR with 0 reviews, admin user
    helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "admin should be blocked when bypass disabled"
    );
}

/// No protection rule on the target branch — merge gates pass (but merge itself
/// needs a real git repo, so we test with a repo).
#[sqlx::test(migrations = "./migrations")]
async fn merge_no_protection_passes(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-no-rule", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Set up real git repo so the merge can actually execute
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    // Create feature branch
    helpers::git_cmd(&work_path, &["checkout", "-b", "feat"]);
    std::fs::write(work_path.join("feature.txt"), "new feature").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add feature"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feat"]);

    // Point project repo_path to the bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // Get head_sha of feat branch
    let head_sha = helpers::git_cmd(&work_path, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Insert MR with real head_sha
    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, 1, $3, 'feat', 'main', 'Test MR', 'open', $4)",
    )
    .bind(mr_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&head_sha)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // NO protection rule — gates should pass, merge should succeed
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "merge with no protection should succeed: {body}"
    );
}

/// Admin bypass enabled: admin can skip approval gate.
#[sqlx::test(migrations = "./migrations")]
async fn merge_admin_bypass(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-bypass", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Set up real git repo
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    helpers::git_cmd(&work_path, &["checkout", "-b", "feat"]);
    std::fs::write(work_path.join("feature.txt"), "bypass feature").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add feature"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feat"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let head_sha = helpers::git_cmd(&work_path, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Protection rule: require 1 approval, admin bypass ENABLED
    helpers::insert_branch_protection(&pool, project_id, "main", 1, &["merge"], &[], false, true)
        .await;

    // Insert MR with 0 reviews
    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, 1, $3, 'feat', 'main', 'Bypass MR', 'open', $4)",
    )
    .bind(mr_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&head_sha)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // Admin should bypass the approval gate
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "admin bypass should allow merge: {body}"
    );
}

/// CI success + protection rule passes.
#[sqlx::test(migrations = "./migrations")]
async fn merge_allowed_ci_success(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-ci-ok", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Set up real git repo
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    helpers::git_cmd(&work_path, &["checkout", "-b", "feat"]);
    std::fs::write(work_path.join("feature.txt"), "ci feature").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add ci feature"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feat"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let head_sha = helpers::git_cmd(&work_path, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Protection rule: require CI success
    helpers::insert_branch_protection(
        &pool,
        project_id,
        "main",
        0,
        &["merge"],
        &["ci"],
        false,
        false,
    )
    .await;

    // Insert successful pipeline
    helpers::insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/feat",
        "push",
    )
    .await;

    // Insert MR
    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, 1, $3, 'feat', 'main', 'CI MR', 'open', $4)",
    )
    .bind(mr_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&head_sha)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "CI success should allow merge: {body}"
    );
}

// ---------------------------------------------------------------------------
// T3: Auto-merge enable/disable
// ---------------------------------------------------------------------------

/// Enable auto-merge on an open MR.
#[sqlx::test(migrations = "./migrations")]
async fn enable_auto_merge(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "auto-merge", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify DB
    let row: (bool,) = sqlx::query_as("SELECT auto_merge FROM merge_requests WHERE id = $1")
        .bind(mr_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(row.0, "auto_merge should be true");
}

/// Disable auto-merge.
#[sqlx::test(migrations = "./migrations")]
async fn disable_auto_merge(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "auto-merge-off", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Enable first
    helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        serde_json::json!({}),
    )
    .await;

    // Disable
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify DB
    let row: (bool,) = sqlx::query_as("SELECT auto_merge FROM merge_requests WHERE id = $1")
        .bind(mr_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!row.0, "auto_merge should be false after disable");
}

/// Enable auto-merge on a closed MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn enable_auto_merge_closed_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "auto-merge-closed", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Close the MR
    sqlx::query("UPDATE merge_requests SET status = 'closed' WHERE id = $1")
        .bind(mr_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "closed MR should not allow auto-merge"
    );
}

/// Auto-merge with specific merge method.
#[sqlx::test(migrations = "./migrations")]
async fn enable_auto_merge_with_method(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "auto-merge-squash", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        serde_json::json!({ "merge_method": "squash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify method stored
    let row: (Option<String>,) =
        sqlx::query_as("SELECT auto_merge_method FROM merge_requests WHERE id = $1")
            .bind(mr_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0.as_deref(), Some("squash"));
}

/// Viewer cannot enable auto-merge.
#[sqlx::test(migrations = "./migrations")]
async fn auto_merge_requires_write(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "auto-merge-perm", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Create viewer user
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "am-viewer", "amviewer@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "viewer",
        Some(project_id),
        &pool,
    )
    .await;

    let (status, _) = helpers::put_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// T36: Squash merge execution
// ---------------------------------------------------------------------------

/// Merge with squash strategy creates a single commit on target.
#[sqlx::test(migrations = "./migrations")]
async fn merge_squash_strategy(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "squash-merge", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Set up git repo with 2 commits on feature branch
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    helpers::git_cmd(&work_path, &["checkout", "-b", "feat-squash"]);
    std::fs::write(work_path.join("file1.txt"), "first").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "first commit"]);
    std::fs::write(work_path.join("file2.txt"), "second").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "second commit"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feat-squash"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let head_sha = helpers::git_cmd(&work_path, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, 1, $3, 'feat-squash', 'main', 'Squash MR', 'open', $4)",
    )
    .bind(mr_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&head_sha)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // Update auto-created "main" protection rule to allow squash merges
    sqlx::query(
        "UPDATE branch_protection_rules SET merge_methods = '{merge,squash}' WHERE project_id = $1 AND pattern = 'main'",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Merge with squash method
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({ "merge_method": "squash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "squash merge failed: {body}");

    // Verify the MR status is now merged
    let row: (String,) = sqlx::query_as("SELECT status FROM merge_requests WHERE id = $1")
        .bind(mr_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "merged");
}

// ---------------------------------------------------------------------------
// Comment CRUD
// ---------------------------------------------------------------------------

/// Update a comment on a merge request.
#[sqlx::test(migrations = "./migrations")]
async fn update_mr_comment(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "commentup", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Insert comment directly
    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("original body")
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        serde_json::json!({ "body": "updated body" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update comment failed: {body}");
    assert_eq!(body["body"], "updated body");
    assert_eq!(body["id"], comment_id.to_string());
}

/// Delete a comment on a merge request.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_comment(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "commentdel", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("to be deleted")
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify deletion
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM comments WHERE id = $1")
        .bind(comment_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);
}

// ---------------------------------------------------------------------------
// Single review retrieval
// ---------------------------------------------------------------------------

/// Get a single review by ID.
#[sqlx::test(migrations = "./migrations")]
async fn get_single_review(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "reviewget", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Insert review directly
    let review_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mr_reviews (id, project_id, mr_id, reviewer_id, verdict, body) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(review_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("approve")
    .bind("looks good")
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews/{review_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get review failed: {body}");
    assert_eq!(body["id"], review_id.to_string());
    assert_eq!(body["verdict"], "approve");
    assert_eq!(body["body"], "looks good");
}

// ---------------------------------------------------------------------------
// Close / Reopen MR
// ---------------------------------------------------------------------------

/// Close an MR then reopen it.
#[sqlx::test(migrations = "./migrations")]
async fn close_and_reopen_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "closereopen", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Close it
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        serde_json::json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "close MR failed: {body}");
    assert_eq!(body["status"], "closed");

    // Reopen it
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        serde_json::json!({ "status": "open" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reopen MR failed: {body}");
    assert_eq!(body["status"], "open");
}

// ---------------------------------------------------------------------------
// Merge already-merged MR
// ---------------------------------------------------------------------------

/// Attempting to merge an already-merged MR returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn merge_already_merged_mr_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "alreadymerged", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Set MR status to merged directly in DB
    sqlx::query("UPDATE merge_requests SET status = 'merged' WHERE project_id = $1 AND number = 1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "expected 400: {body}");
}

// ---------------------------------------------------------------------------
// T4: require_up_to_date gate
// ---------------------------------------------------------------------------

/// Merge blocked when require_up_to_date is true and target has newer commits.
#[sqlx::test(migrations = "./migrations")]
async fn merge_blocked_require_up_to_date(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "gate-up-to-date", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Set up real git repo
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    // Create feature branch from current main
    helpers::git_cmd(&work_path, &["checkout", "-b", "feat-stale"]);
    std::fs::write(work_path.join("feature.txt"), "feature work").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "feature commit"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feat-stale"]);

    // Now add a commit to main (making feat-stale not up-to-date)
    helpers::git_cmd(&work_path, &["checkout", "main"]);
    std::fs::write(work_path.join("hotfix.txt"), "hotfix").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "hotfix on main"]);
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    // Point project repo_path to the bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let head_sha = helpers::git_cmd(&work_path, &["rev-parse", "feat-stale"])
        .trim()
        .to_string();

    // Protection rule: require_up_to_date = true
    helpers::insert_branch_protection(&pool, project_id, "main", 0, &["merge"], &[], true, false)
        .await;

    // Insert MR
    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, 1, $3, 'feat-stale', 'main', 'Stale MR', 'open', $4)",
    )
    .bind(mr_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&head_sha)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // Merge should be blocked because source is not up to date with target
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should block stale merge: {body}"
    );
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.to_lowercase().contains("up to date"),
        "error should mention up to date, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// T5: Rebase merge strategy
// ---------------------------------------------------------------------------

/// Merge with rebase strategy (fast-forward) succeeds when source is on top of target.
#[sqlx::test(migrations = "./migrations")]
async fn merge_rebase_strategy(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "rebase-merge", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;

    // Set up git repo — feature branch must be fast-forwardable onto main
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    helpers::git_cmd(&work_path, &["checkout", "-b", "feat-rebase"]);
    std::fs::write(work_path.join("rebase.txt"), "rebase feature").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "rebase commit"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feat-rebase"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let head_sha = helpers::git_cmd(&work_path, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Update protection rule to allow rebase merges
    helpers::insert_branch_protection(
        &pool,
        project_id,
        "main",
        0,
        &["merge", "rebase"],
        &[],
        false,
        false,
    )
    .await;

    // Insert MR
    let mr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, 1, $3, 'feat-rebase', 'main', 'Rebase MR', 'open', $4)",
    )
    .bind(mr_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&head_sha)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 2 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        serde_json::json!({ "merge_method": "rebase" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rebase merge should succeed: {body}"
    );
    assert_eq!(body["status"], "merged");
}

// ---------------------------------------------------------------------------
// Auto-merge on nonexistent MR
// ---------------------------------------------------------------------------

/// Disable auto-merge on a nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn auto_merge_nonexistent_mr_disable(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "auto-merge-noexist", "public").await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/auto-merge"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
