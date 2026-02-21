mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E8: Notification Integration Tests (10 tests)
// ---------------------------------------------------------------------------

/// Insert a test notification directly into the database.
async fn insert_notification(
    pool: &PgPool,
    user_id: Uuid,
    subject: &str,
    status: &str,
    notification_type: &str,
) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO notifications (user_id, notification_type, subject, channel, status) \
         VALUES ($1, $2, $3, 'in_app', $4) \
         RETURNING id",
    )
    .bind(user_id)
    .bind(notification_type)
    .bind(subject)
    .bind(status)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

#[sqlx::test(migrations = "./migrations")]
async fn list_notifications_empty(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/notifications").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_notifications_with_data(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Get admin user ID
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Insert notifications directly
    insert_notification(&pool, admin_id, "Test 1", "pending", "info").await;
    insert_notification(&pool, admin_id, "Test 2", "sent", "info").await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/notifications").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_notifications_pagination(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    for i in 1..=5 {
        insert_notification(&pool, admin_id, &format!("Note {i}"), "pending", "info").await;
    }

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/notifications?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_notifications_filter_status(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    insert_notification(&pool, admin_id, "Unread", "pending", "info").await;
    insert_notification(&pool, admin_id, "Read", "read", "info").await;

    // Filter by status=pending
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/notifications?status=pending").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["subject"], "Unread");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_notifications_filter_type(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    insert_notification(&pool, admin_id, "Alert", "pending", "alert").await;
    insert_notification(&pool, admin_id, "Info", "pending", "info").await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/notifications?type=alert").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["subject"], "Alert");
}

#[sqlx::test(migrations = "./migrations")]
async fn unread_count(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    insert_notification(&pool, admin_id, "N1", "pending", "info").await;
    insert_notification(&pool, admin_id, "N2", "sent", "info").await;
    insert_notification(&pool, admin_id, "N3", "read", "info").await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/notifications/unread-count").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 2); // pending + sent = unread
}

#[sqlx::test(migrations = "./migrations")]
async fn mark_notification_read(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let notif_id = insert_notification(&pool, admin_id, "Mark Me", "pending", "info").await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/notifications/{notif_id}/read"),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    // Verify unread count decreased
    let (_, count_body) =
        helpers::get_json(&app, &admin_token, "/api/notifications/unread-count").await;
    assert_eq!(count_body["count"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn mark_already_read_notification(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let notif_id = insert_notification(&pool, admin_id, "Already Read", "read", "info").await;

    // Mark already-read notification — should return 404 (no rows affected)
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/notifications/{notif_id}/read"),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn notification_scoped_to_user(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Create another user
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "notifuser", "notifuser@test.com").await;

    // Get admin and user IDs
    let (_, me_admin) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me_admin["id"].as_str().unwrap()).unwrap();

    // Insert notification for admin
    insert_notification(&pool, admin_id, "Admin Only", "pending", "info").await;

    // Other user should see 0 notifications
    let (status, body) = helpers::get_json(&app, &user_token, "/api/notifications").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn cannot_mark_other_users_notification(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me_admin) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me_admin["id"].as_str().unwrap()).unwrap();

    let notif_id = insert_notification(&pool, admin_id, "Admin Notif", "pending", "info").await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "notifstealer", "steal@test.com").await;

    // User tries to mark admin's notification as read — 404
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/notifications/{notif_id}/read"),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}
