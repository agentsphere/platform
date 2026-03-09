#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

use fred::interfaces::ClientLike;
use platform::config::Config;
use platform::store::AppState;

/// Build a test `AppState` with optional `cli_spawn_enabled`.
///
/// Wrapper around `test_state()` that optionally enables CLI subprocess spawning.
/// `CLAUDE_CLI_PATH` must be set externally (by `hack/test-in-cluster.sh`) to point
/// to `tests/fixtures/mock-claude-cli.sh`.
///
/// - `cli_spawn_enabled = false` — tests that don't exercise CLI subprocess flow
/// - `cli_spawn_enabled = true` — tests that trigger the mock CLI subprocess
pub async fn test_state_with_cli(pool: PgPool, cli_spawn_enabled: bool) -> (AppState, String) {
    let (mut state, token) = test_state(pool).await;
    if cli_spawn_enabled {
        let mut config = (*state.config).clone();
        config.cli_spawn_enabled = true;
        state.config = Arc::new(config);
    }
    (state, token)
}

/// Build a test `AppState` and pre-authenticated admin API token.
///
/// - Bootstraps permissions, roles, admin user (password = "testpassword")
/// - Connects to real Valkey (no FLUSHDB — keys are UUID-scoped)
/// - Connects to real `MinIO` (S3 backend, bucket: `platform-e2e`)
/// - Creates an API token for the admin user directly in the DB
/// - Uses a real `Kube` client (Kind cluster must be running)
///
/// Returns `(state, admin_token)`. The admin token bypasses the login
/// endpoint's rate limiter, which was the only source of cross-test
/// Valkey key collision (`rate:login:admin`).
#[allow(clippy::too_many_lines)]
pub async fn test_state(pool: PgPool) -> (AppState, String) {
    // Ensure a rustls CryptoProvider is installed (needed by reqwest/fred)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Bootstrap seed data
    platform::store::bootstrap::run(&pool, Some("testpassword"), true)
        .await
        .expect("bootstrap failed");

    // Connect to real Valkey — no FLUSHDB needed. All Valkey keys are UUID-scoped
    // (permission cache, upload sessions, WebAuthn challenges) and never collide
    // between parallel tests. The admin token is created directly in the DB,
    // bypassing the login endpoint's rate limiter.
    //
    // Pool size 1 (not 4) — with 32 parallel tests, 4×32=128 connections overwhelm
    // the server under load. 1×32=32 is sufficient for test workloads.
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey_config =
        fred::types::config::Config::from_url(&valkey_url).expect("invalid VALKEY_URL");
    let valkey = fred::clients::Pool::new(valkey_config, None, None, None, 1)
        .expect("valkey pool creation failed");
    valkey.init().await.expect("valkey connection failed");

    // Real MinIO (S3 backend) — same instance as Postgres/Valkey from Kind cluster.
    // Uses a dedicated test bucket to avoid polluting production data.
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

    // Real Kube client — integration tests require a Kind cluster
    let kube = kube::Client::try_default()
        .await
        .expect("kube client required — run `just cluster-up` first");

    // Namespace prefix from test-in-cluster.sh (for test isolation)
    let ns_prefix = std::env::var("PLATFORM_NS_PREFIX").ok();
    let pipeline_namespace =
        std::env::var("PLATFORM_PIPELINE_NAMESPACE").unwrap_or_else(|_| "test-pipelines".into());
    let agent_namespace =
        std::env::var("PLATFORM_AGENT_NAMESPACE").unwrap_or_else(|_| "test-agents".into());
    let registry_url = std::env::var("PLATFORM_REGISTRY_URL").ok();
    let platform_api_url = std::env::var("PLATFORM_API_URL")
        .unwrap_or_else(|_| "http://platform.test-agents.svc.cluster.local:8080".into());
    let valkey_agent_host =
        std::env::var("PLATFORM_VALKEY_AGENT_HOST").unwrap_or_else(|_| "localhost:6379".into());

    // Config with test defaults
    let config = Config {
        listen: "127.0.0.1:0".into(),
        database_url: "postgres://localhost/test".into(),
        valkey_url,
        minio_endpoint: minio_endpoint.clone(),
        minio_access_key: minio_access_key.clone(),
        minio_secret_key: minio_secret_key.clone(),
        master_key: Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
        git_repos_path: std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4())),
        ops_repos_path: std::env::temp_dir().join(format!("platform-ops-{}", Uuid::new_v4())),
        smtp_host: None,
        smtp_port: 587,
        smtp_from: "test@localhost".into(),
        smtp_username: None,
        smtp_password: None,
        admin_password: None,
        pipeline_namespace,
        agent_namespace,
        registry_url,
        secure_cookies: false,
        cors_origins: vec![],
        trust_proxy_headers: false,
        dev_mode: true,
        webauthn_rp_id: "localhost".into(),
        webauthn_rp_origin: "http://localhost:8080".into(),
        permission_cache_ttl_secs: 300,
        webauthn_rp_name: "Test Platform".into(),
        platform_api_url,
        platform_namespace: "test-platform".into(),
        ssh_listen: None,
        ssh_host_key_path: "/tmp/test_ssh_host_key".into(),
        max_cli_subprocesses: 10,
        valkey_agent_host,
        agent_runner_dir: std::env::var("PLATFORM_AGENT_RUNNER_DIR").map_or_else(
            |_| std::env::temp_dir().join(format!("agent-runner-{}", Uuid::new_v4())),
            PathBuf::from,
        ),
        claude_cli_version: "stable".into(),
        ns_prefix,
        cli_spawn_enabled: false,
        registry_node_url: std::env::var("PLATFORM_REGISTRY_NODE_URL").ok(),
        seed_images_path: std::env::var("PLATFORM_SEED_IMAGES_PATH")
            .map_or_else(|_| "/tmp/seed-images".into(), std::path::PathBuf::from),
        health_check_interval_secs: 15,
        self_observe_level: "warn".into(),
    };

    // Seed registry images from OCI tarballs (idempotent, uses file-based cache)
    if let Err(e) =
        platform::registry::seed::seed_all(&pool, &minio, &config.seed_images_path).await
    {
        tracing::warn!(error = %e, "test registry seed failed (non-fatal)");
    }

    // Build WebAuthn
    let webauthn = platform::auth::passkey::build_webauthn(&config).expect("webauthn build failed");

    let state = AppState {
        pool,
        valkey,
        minio,
        kube,
        config: Arc::new(config.clone()),
        webauthn: Arc::new(webauthn),
        pipeline_notify: Arc::new(tokio::sync::Notify::new()),
        deploy_notify: Arc::new(tokio::sync::Notify::new()),
        secret_requests: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        cli_sessions: platform::agent::claude_cli::CliSessionManager::new(
            config.max_cli_subprocesses,
        ),
        health: Arc::new(std::sync::RwLock::new(
            platform::health::HealthSnapshot::default(),
        )),
        task_registry: Arc::new(platform::health::TaskRegistry::new()),
    };

    // Create an API token for the bootstrap admin directly in the DB,
    // bypassing the login endpoint and its rate limiter.
    let admin_row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&state.pool)
        .await
        .expect("admin user must exist after bootstrap");

    let (raw_token, token_hash) = platform::auth::token::generate_api_token();
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at)
         VALUES ($1, 'test-admin', $2, now() + interval '1 day')",
    )
    .bind(admin_row.0)
    .bind(&token_hash)
    .execute(&state.pool)
    .await
    .expect("create admin api token");

    (state, raw_token)
}

/// Build the full API router with the given state.
///
/// Includes the main API router plus observe (query + alerts) and registry routers.
/// The observe ingest routes (OTLP) are omitted since they require background channels.
pub fn test_router(state: AppState) -> Router {
    let ready_state = state.clone();
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route(
            "/readyz",
            axum::routing::get(move || {
                let s = ready_state.clone();
                async move {
                    if platform::health::checks::is_ready(&s).await {
                        (axum::http::StatusCode::OK, "ok")
                    } else {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
                    }
                }
            }),
        )
        .merge(platform::api::router())
        .merge(platform::observe::query::router())
        .merge(platform::observe::alert::router())
        .merge(platform::registry::router())
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

/// Create a project (DB only, no K8s namespaces). Returns the project id.
pub async fn create_project(app: &Router, token: &str, name: &str, visibility: &str) -> Uuid {
    let (status, body) = post_json(
        app,
        token,
        "/api/projects",
        serde_json::json!({
            "name": name,
            "visibility": visibility,
            "setup_infra": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create project failed: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Create a project with full infrastructure (K8s namespaces + ops repo).
pub async fn create_project_with_infra(
    app: &Router,
    token: &str,
    name: &str,
    visibility: &str,
) -> Uuid {
    let (status, body) = post_json(
        app,
        token,
        "/api/projects",
        serde_json::json!({
            "name": name,
            "visibility": visibility,
            "setup_infra": true,
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

/// Store a dummy Anthropic API key for a user (required for create-app sessions).
pub async fn set_user_api_key(pool: &PgPool, user_id: Uuid) {
    let master_key = platform::secrets::engine::parse_master_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    platform::secrets::user_keys::set_user_key(
        pool,
        &master_key,
        user_id,
        "anthropic",
        "sk-ant-test-dummy-key",
    )
    .await
    .expect("set_user_key failed");
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

/// Start a real TCP server for integration tests that need pod connectivity.
///
/// Binds to `PLATFORM_LISTEN_PORT` (set by `test-in-cluster.sh`).
/// Returns `(state, admin_token, server_handle)`.
pub async fn start_test_server(pool: PgPool) -> (AppState, String, tokio::task::JoinHandle<()>) {
    let port: u16 = std::env::var("PLATFORM_LISTEN_PORT")
        .expect("PLATFORM_LISTEN_PORT must be set — run via: just test-integration")
        .parse()
        .expect("invalid PLATFORM_LISTEN_PORT");
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind listener");
    let (state, token) = test_state(pool).await;
    let app = test_router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, token, handle)
}

/// Extract JSON body from a response.
async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Raw bytes HTTP helper
// ---------------------------------------------------------------------------

/// Send a raw GET request and return the body as bytes (for non-JSON endpoints).
pub async fn get_bytes(app: &Router, token: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(path);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

// ---------------------------------------------------------------------------
// Git repo helpers
// ---------------------------------------------------------------------------

/// Create a bare git repo in a tempdir, return `(TempDir, PathBuf)`.
///
/// Uses `/tmp/platform-e2e/` as the base directory so that repos are visible
/// inside the Kind cluster (which has `/tmp/platform-e2e` as an extra mount).
pub fn create_bare_repo() -> (TempDir, PathBuf) {
    let base = Path::new("/tmp/platform-e2e");
    std::fs::create_dir_all(base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let repo_path = dir.path().join("test.git");
    let output = std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&repo_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init --bare failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (dir, repo_path)
}

/// Create a working copy from a bare repo with an initial commit.
/// Returns `(TempDir, PathBuf)`.
pub fn create_working_copy(bare_path: &Path) -> (TempDir, PathBuf) {
    let base = Path::new("/tmp/platform-e2e");
    std::fs::create_dir_all(base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let work_path = dir.path().join("work");
    git_cmd_at(dir.path(), &["clone", bare_path.to_str().unwrap(), "work"]);

    // Configure git user for commits
    git_cmd(&work_path, &["config", "user.email", "test@e2e.local"]);
    git_cmd(&work_path, &["config", "user.name", "E2E Test"]);

    // Create initial commit
    std::fs::write(work_path.join("README.md"), "# Test Project\n").unwrap();
    git_cmd(&work_path, &["add", "."]);
    git_cmd(&work_path, &["commit", "-m", "initial commit"]);
    git_cmd(&work_path, &["push", "origin", "HEAD:refs/heads/main"]);
    (dir, work_path)
}

/// Run a git command in a directory; panic on failure.
pub fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "E2E Test")
        .env("GIT_AUTHOR_EMAIL", "test@e2e.local")
        .env("GIT_COMMITTER_NAME", "E2E Test")
        .env("GIT_COMMITTER_EMAIL", "test@e2e.local")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Run a git command in a directory (no env override).
fn git_cmd_at(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
