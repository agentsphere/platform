mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// E7: Webhook Integration Tests (12 tests)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-proj", "public").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["push", "issue"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
    assert_eq!(body["url"], "https://example.com/hook");
    let events = body["events"].as_array().unwrap();
    assert!(events.iter().any(|e| e == "push"));
    assert!(events.iter().any(|e| e == "issue"));
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_invalid_url(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-invalid", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "ftp://example.com/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_ssrf_localhost(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-ssrf-lo", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "http://localhost/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_ssrf_private_10(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-ssrf-10", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "http://10.0.0.1/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_ssrf_private_172(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-ssrf-172", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "http://172.16.0.1/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_ssrf_private_192(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-ssrf-192", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "http://192.168.1.1/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_ssrf_metadata(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-ssrf-meta", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "http://169.254.169.254/",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_ssrf_ipv6_loopback(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-ssrf-ipv6", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "http://[::1]/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_webhooks(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-list", "public").await;

    for i in 1..=2 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/webhooks"),
            serde_json::json!({
                "url": format!("https://example.com/hook{i}"),
                "events": ["push"],
            }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_and_delete_webhook(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-upd-del", "public").await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/original",
            "events": ["push"],
        }),
    )
    .await;
    let wh_id = create_body["id"].as_str().unwrap();

    // Update URL
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
        serde_json::json!({ "url": "https://example.com/updated" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["url"], "https://example.com/updated");

    // Delete
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify gone
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn webhook_requires_project_write(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-no-write", "public").await;

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "whviewer", "whviewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Viewer cannot create webhooks (requires project:write)
    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["push"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn webhook_secret_not_exposed(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-secret", "public").await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/secret-hook",
            "events": ["push"],
            "secret": "my-super-secret",
        }),
    )
    .await;
    let wh_id = create_body["id"].as_str().unwrap();

    // GET the webhook back — secret should not be in the response
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The WebhookResponse struct does not include secret field
    assert!(body.get("secret").is_none() || body["secret"].is_null());
}
