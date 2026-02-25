mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E7: Webhook Integration Tests (22 tests)
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
    let items = body["items"].as_array().expect("items should be array");
    assert_eq!(items.len(), 2);
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

// ---------------------------------------------------------------------------
// Additional webhook coverage tests
// ---------------------------------------------------------------------------

/// Helper: insert webhook directly into DB (bypasses SSRF validation).
async fn insert_webhook(pool: &PgPool, project_id: Uuid, url: &str, events: &[&str]) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO webhooks (id, project_id, url, events, active) VALUES ($1,$2,$3,$4,true)",
    )
    .bind(id)
    .bind(project_id)
    .bind(url)
    .bind(events)
    .execute(pool)
    .await
    .expect("insert webhook");
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_invalid_event(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-bad-event", "public").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["push", "nonexistent_event"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("invalid event"),
        "error should mention invalid event, got: {msg}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn create_webhook_empty_events(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-empty-ev", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": [],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_single_webhook(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-get-one", "public").await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/single",
            "events": ["push", "mr"],
        }),
    )
    .await;
    let wh_id = create_body["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], wh_id);
    assert_eq!(body["url"], "https://example.com/single");
    assert_eq!(body["active"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_webhook_not_found(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-get-404", "public").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{fake_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_webhook_not_found(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-del-404", "public").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{fake_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_webhook_events(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-upd-events", "public").await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["push"],
        }),
    )
    .await;
    let wh_id = create_body["id"].as_str().unwrap();

    // Update events from [push] to [mr, build, deploy]
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
        serde_json::json!({ "events": ["mr", "build", "deploy"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert!(events.iter().any(|e| e == "mr"));
    assert!(events.iter().any(|e| e == "build"));
    assert!(events.iter().any(|e| e == "deploy"));
}

#[sqlx::test(migrations = "./migrations")]
async fn update_webhook_invalid_event(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-upd-bad", "public").await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["push"],
        }),
    )
    .await;
    let wh_id = create_body["id"].as_str().unwrap();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
        serde_json::json!({ "events": ["bogus_event"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_webhook_deactivate(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-deactivate", "public").await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks"),
        serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["push"],
        }),
    )
    .await;
    let wh_id = create_body["id"].as_str().unwrap();
    assert_eq!(create_body["active"], true);

    // Deactivate
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}"),
        serde_json::json!({ "active": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_webhook_endpoint(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-test-ep", "public").await;

    // Insert webhook directly to bypass SSRF checks (test endpoint will try to deliver)
    let wh_id = insert_webhook(
        &pool,
        project_id,
        "https://example.com/test-delivery",
        &["push"],
    )
    .await;

    // Test webhook endpoint should return OK (delivery happens async, may fail, but endpoint itself succeeds)
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{wh_id}/test"),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "test webhook failed: {body}");
    assert_eq!(body["ok"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_webhook_not_found(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "wh-test-404", "public").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/webhooks/{fake_id}/test"),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}
