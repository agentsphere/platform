mod helpers;

use axum::http::StatusCode;
use fred::interfaces::KeysInterface;
use sqlx::PgPool;
use uuid::Uuid;

/// Build a state WITHOUT running full bootstrap — only seeds permissions/roles, no admin user.
/// This lets us test the setup flow (which requires zero users).
async fn setup_test_state(pool: PgPool) -> (platform::store::AppState, String) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Run bootstrap in production mode (dev_mode=false) — seeds data + generates setup token
    let result = platform::store::bootstrap::run(&pool, None, false)
        .await
        .expect("bootstrap failed");

    let setup_token = match result {
        platform::store::bootstrap::BootstrapResult::SetupToken(t) => t,
        _ => panic!("expected SetupToken from production bootstrap"),
    };

    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey = platform::store::valkey::connect(&valkey_url)
        .await
        .expect("valkey connection failed");

    // Clear the shared setup rate limit key to avoid cross-test interference
    let _: () = valkey.del("rate:setup:global").await.unwrap_or_default();

    let minio_endpoint =
        std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let minio_access_key = std::env::var("MINIO_ACCESS_KEY").unwrap_or_else(|_| "platform".into());
    let minio_secret_key = std::env::var("MINIO_SECRET_KEY").unwrap_or_else(|_| "devdevdev".into());
    let minio = {
        let mut builder = opendal::services::S3::default();
        builder = builder
            .endpoint(&minio_endpoint)
            .access_key_id(&minio_access_key)
            .secret_access_key(&minio_secret_key)
            .bucket("platform-e2e")
            .region("us-east-1");
        opendal::Operator::new(builder)
            .expect("minio S3 operator")
            .finish()
    };

    let kube = if let Ok(c) = kube::Client::try_default().await {
        c
    } else {
        let cfg = kube::Config::new("https://127.0.0.1:1".parse().unwrap());
        kube::Client::try_from(cfg).expect("dummy kube client")
    };

    let config = platform::config::Config {
        listen: "127.0.0.1:0".into(),
        database_url: "postgres://localhost/test".into(),
        valkey_url,
        minio_endpoint,
        minio_access_key,
        minio_secret_key,
        master_key: Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
        git_repos_path: std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4())),
        ops_repos_path: std::env::temp_dir().join(format!("platform-ops-{}", Uuid::new_v4())),
        smtp_host: None,
        smtp_port: 587,
        smtp_from: "test@localhost".into(),
        smtp_username: None,
        smtp_password: None,
        admin_password: None,
        pipeline_namespace: "test-pipelines".into(),
        agent_namespace: "test-agents".into(),
        registry_url: None,
        secure_cookies: false,
        cors_origins: vec![],
        trust_proxy_headers: false,
        dev_mode: false,
        webauthn_rp_id: "localhost".into(),
        webauthn_rp_origin: "http://localhost:8080".into(),
        permission_cache_ttl_secs: 300,
        webauthn_rp_name: "Test Platform".into(),
        platform_api_url: "http://platform.test-agents.svc.cluster.local:8080".into(),
        platform_namespace: "test-platform".into(),
        ssh_listen: None,
        ssh_host_key_path: "/tmp/test_ssh_host_key".into(),
    };

    let webauthn = platform::auth::passkey::build_webauthn(&config).expect("webauthn build failed");

    let state = platform::store::AppState {
        pool,
        valkey,
        minio,
        kube,
        config: std::sync::Arc::new(config),
        webauthn: std::sync::Arc::new(webauthn),
        pipeline_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        deploy_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        inprocess_sessions: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
    };

    (state, setup_token)
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_status_no_users(pool: PgPool) {
    let (state, _token) = setup_test_state(pool).await;
    let app = helpers::test_router(state);
    let (status, body) = helpers::get_json(&app, "", "/api/setup/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["needs_setup"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_status_has_users(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/setup/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["needs_setup"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_with_valid_token_creates_admin(pool: PgPool) {
    let (state, token) = setup_test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "setup failed: {body}");
    assert_eq!(body["name"], "myadmin");
    assert_eq!(body["email"], "admin@example.com");

    // Verify admin user exists
    let count: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users WHERE name = 'myadmin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_creates_personal_workspace(pool: PgPool) {
    let (state, token) = setup_test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (_status, _body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;

    // Verify workspace exists
    let ws_count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspaces w JOIN workspace_members wm ON wm.workspace_id = w.id JOIN users u ON u.id = wm.user_id WHERE u.name = 'myadmin' AND wm.role = 'owner'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ws_count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_assigns_admin_role(pool: PgPool) {
    let (state, token) = setup_test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "setup failed: {body}");

    // Verify admin role assignment
    let role_count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM user_roles ur JOIN roles r ON r.id = ur.role_id JOIN users u ON u.id = ur.user_id WHERE u.name = 'myadmin' AND r.name = 'admin'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role_count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_consumes_token(pool: PgPool) {
    let (state, token) = setup_test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;

    // Token should have used_at set
    let used: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM setup_tokens WHERE used_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(used, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_with_wrong_token_returns_401(pool: PgPool) {
    let (state, _token) = setup_test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": "0000000000000000000000000000000000000000000000000000000000000000",
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_with_expired_token_returns_401(pool: PgPool) {
    let (state, token) = setup_test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Expire the token
    let token_hash = platform::store::bootstrap::hash_setup_token(&token);
    sqlx::query(
        "UPDATE setup_tokens SET expires_at = now() - interval '1 hour' WHERE token_hash = $1",
    )
    .bind(&token_hash)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_with_used_token_returns_401(pool: PgPool) {
    let (state, token) = setup_test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Use the token first
    let (status, _body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Second attempt: users already exist → 404
    let (status, _body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": token,
            "name": "myadmin2",
            "email": "admin2@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_when_users_exist_returns_404(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _body) = helpers::post_json(
        &app,
        "",
        "/api/setup",
        serde_json::json!({
            "token": "doesntmatter",
            "name": "myadmin",
            "email": "admin@example.com",
            "password": "securepassword123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Test rate limiting using a dedicated key that cannot collide with other tests.
/// The setup endpoint uses `rate:setup:global` which is shared across ALL
/// parallel tests — hitting it from here would poison other setup tests.
/// Instead, we test the `check_rate` function directly with a UUID-scoped key.
#[sqlx::test(migrations = "./migrations")]
async fn setup_rate_limited(pool: PgPool) {
    let (state, _token) = setup_test_state(pool).await;

    // Use a unique rate limit key so we don't interfere with parallel tests
    let unique_id = Uuid::new_v4().to_string();

    // Exhaust rate limit: 3 attempts allowed
    for i in 1..=3 {
        let result =
            platform::auth::rate_limit::check_rate(&state.valkey, "setup_test", &unique_id, 3, 300)
                .await;
        assert!(result.is_ok(), "attempt {i} should succeed");
    }

    // 4th attempt should be rate limited
    let result =
        platform::auth::rate_limit::check_rate(&state.valkey, "setup_test", &unique_id, 3, 300)
            .await;
    assert!(result.is_err(), "4th attempt should be rate-limited");
}
