#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use platform::config::Config;
use platform::store::AppState;

/// Build a test `AppState` from the given pool.
///
/// - Bootstraps permissions, roles, admin user (password = "testpassword")
/// - Connects to real Valkey (flushes DB to prevent cross-test pollution)
/// - Uses in-memory object storage (no `MinIO` required)
/// - Uses a dummy `Kube` client (panics if actually called)
pub async fn test_state(pool: PgPool) -> AppState {
    // Bootstrap seed data
    platform::store::bootstrap::run(&pool, Some("testpassword"))
        .await
        .expect("bootstrap failed");

    // Connect to real Valkey and flush
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey = platform::store::valkey::connect(&valkey_url)
        .await
        .expect("valkey connection failed");
    // Flush Valkey to prevent cross-test pollution.
    {
        use fred::interfaces::ClientLike;
        let _: fred::types::Value = valkey
            .custom(
                fred::types::CustomCommand::new_static("FLUSHDB", None, false),
                Vec::<fred::types::Value>::new(),
            )
            .await
            .expect("FLUSHDB failed");
    }

    // In-memory MinIO
    let minio = opendal::Operator::new(opendal::services::Memory::default())
        .expect("memory operator")
        .finish();

    // Dummy Kube client — try real kubeconfig, fall back to a stub
    let kube = if let Ok(c) = kube::Client::try_default().await {
        c
    } else {
        // No kubeconfig available in CI — build a stub config manually.
        // It will panic if any test actually makes a K8s API call.
        let cfg = kube::Config::new("https://127.0.0.1:1".parse().unwrap());
        kube::Client::try_from(cfg).expect("dummy kube client")
    };

    // Config with test defaults
    let config = Config {
        listen: "127.0.0.1:0".into(),
        database_url: "postgres://localhost/test".into(),
        valkey_url,
        minio_endpoint: "http://localhost:9000".into(),
        minio_access_key: "test".into(),
        minio_secret_key: "test".into(),
        master_key: None,
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
        dev_mode: true,
        webauthn_rp_id: "localhost".into(),
        webauthn_rp_origin: "http://localhost:8080".into(),
        webauthn_rp_name: "Test Platform".into(),
    };

    // Build WebAuthn
    let webauthn = platform::auth::passkey::build_webauthn(&config).expect("webauthn build failed");

    AppState {
        pool,
        valkey,
        minio,
        kube,
        config: Arc::new(config),
        webauthn: Arc::new(webauthn),
    }
}

/// Build the full API router with the given state.
pub fn test_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(platform::api::router())
        .with_state(state)
}

/// Login as the bootstrap admin user. Returns the bearer token.
pub async fn admin_login(app: &Router) -> String {
    let (status, body) = post_json(
        app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin login failed: {body}");
    body["token"]
        .as_str()
        .expect("login response missing token")
        .to_owned()
}

/// Create a user via admin API, login with them, return `(user_id, token)`.
pub async fn create_user(
    app: &Router,
    admin_token: &str,
    name: &str,
    email: &str,
) -> (Uuid, String) {
    let password = "testpass123";
    let (status, body) = post_json(
        app,
        admin_token,
        "/api/users",
        serde_json::json!({
            "name": name,
            "email": email,
            "password": password,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create user failed: {body}");
    let user_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    // Login with the new user
    let (status, login_body) = post_json(
        app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": name, "password": password }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "user login failed: {login_body}");
    let token = login_body["token"].as_str().unwrap().to_owned();

    (user_id, token)
}

/// Create a project. Returns the project id.
pub async fn create_project(app: &Router, token: &str, name: &str, visibility: &str) -> Uuid {
    let (status, body) = post_json(
        app,
        token,
        "/api/projects",
        serde_json::json!({
            "name": name,
            "visibility": visibility,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create project failed: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Assign a role to a user. Looks up the role by name.
pub async fn assign_role(
    app: &Router,
    admin_token: &str,
    user_id: Uuid,
    role_name: &str,
    project_id: Option<Uuid>,
    pool: &PgPool,
) {
    // Look up role ID by name (use runtime query, not macro, since this is in tests/)
    let row: (Uuid,) = sqlx::query_as("SELECT id FROM roles WHERE name = $1")
        .bind(role_name)
        .fetch_one(pool)
        .await
        .expect("role not found");
    let role_id = row.0;

    let (status, body) = post_json(
        app,
        admin_token,
        &format!("/api/admin/users/{user_id}/roles"),
        serde_json::json!({
            "role_id": role_id,
            "project_id": project_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "assign role failed: {body}");
}

/// Send a GET request with Bearer auth.
pub async fn get_json(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Send a POST request with Bearer auth and JSON body.
pub async fn post_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json");
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Send a PATCH request with Bearer auth and JSON body.
pub async fn patch_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("PATCH")
        .uri(path)
        .header("Content-Type", "application/json");
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Send a PUT request with Bearer auth and JSON body.
pub async fn put_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(path)
        .header("Content-Type", "application/json");
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Send a DELETE request with Bearer auth.
pub async fn delete_json(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("DELETE").uri(path);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Extract JSON body from a response.
async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}
