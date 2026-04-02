// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

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

// ---------------------------------------------------------------------------
// Test-name-aware JSON tracing
// ---------------------------------------------------------------------------

use std::sync::Once;

static INIT_TRACING: Once = Once::new();

/// Initialize test tracing: JSON output to `TEST_LOG_FILE` with `threadName` per line.
/// nextest names each thread after the test, so `threadName` == test name.
pub fn init_test_tracing() {
    INIT_TRACING.call_once(|| {
        if let Ok(path) = std::env::var("TEST_LOG_FILE") {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open test log file");
            tracing_subscriber::fmt()
                .json()
                .with_thread_names(true)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "warn".into()),
                )
                .with_writer(std::sync::Mutex::new(file))
                .try_init()
                .ok();
        }
    });
}

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
    init_test_tracing();
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
    let minio_insecure = std::env::var("MINIO_INSECURE").ok().as_deref() == Some("true");
    let minio = {
        let mut builder = opendal::services::S3::default();
        builder = builder
            .endpoint(&minio_endpoint)
            .access_key_id(&minio_access_key)
            .secret_access_key(&minio_secret_key)
            .bucket("platform-e2e")
            .region("us-east-1");
        let op = opendal::Operator::new(builder)
            .expect("minio S3 operator")
            .finish();
        // S55: accept self-signed TLS certs for MinIO in dev/test
        if minio_insecure {
            let client = reqwest_opendal::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("insecure reqwest client");
            op.layer(opendal::layers::HttpClientLayer::new(
                opendal::raw::HttpClient::with(client),
            ))
        } else {
            op
        }
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
        minio_insecure,
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
        seed_commands_path: std::env::var("PLATFORM_SEED_COMMANDS_PATH")
            .map_or_else(|_| "/tmp/seed-commands".into(), std::path::PathBuf::from),
        health_check_interval_secs: 15,
        self_observe_level: "warn".into(),
        session_idle_timeout_secs: 1800,
        preview_proxy_url: std::env::var("PLATFORM_PREVIEW_PROXY_URL").ok(),
        pipeline_max_parallel: 4,
        mcp_servers_tarball: std::env::var("PLATFORM_MCP_SERVERS_TARBALL")
            .map_or_else(|_| "/tmp/mcp-servers.tar.gz".into(), PathBuf::from),
        gateway_name: std::env::var("PLATFORM_GATEWAY_NAME")
            .unwrap_or_else(|_| "platform-gateway".into()),
        gateway_namespace: std::env::var("PLATFORM_GATEWAY_NAMESPACE")
            .unwrap_or_else(|_| "envoy-gateway-system".into()),
        pipeline_timeout_secs: 3600,
        max_lfs_object_bytes: 5_368_709_120,
        token_max_expiry_days: 365,
        observe_retention_days: 30,
        master_key_previous: None,
        trust_proxy_cidrs: vec![],
        runner_image: "platform-runner:v1".into(),
        git_clone_image: "alpine/git:2.47.2".into(),
        kaniko_image: "gcr.io/kaniko-project/executor:v1.23.2-debug".into(),
        registry_proxy_blobs: false,
        mcp_servers_path: "mcp/servers".into(),
        max_artifact_file_bytes: 50 * 1024 * 1024,
        max_artifact_total_bytes: 500 * 1024 * 1024,
        mesh_enabled: false,
        mesh_ca_cert_ttl_secs: 3600,
        mesh_ca_root_ttl_days: 365,
        proxy_binary_path: None,
    };

    // Seed registry images from OCI tarballs (idempotent, uses file-based cache)
    if let Err(e) =
        platform::registry::seed::seed_all(&pool, &minio, &config.seed_images_path).await
    {
        tracing::warn!(error = %e, "test registry seed failed (non-fatal)");
    }

    // Build WebAuthn
    let webauthn = platform::auth::passkey::build_webauthn(&config).expect("webauthn build failed");

    let audit_tx = platform::audit::AuditLog::new(pool.clone());

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
        cli_auth_manager: Arc::new(platform::onboarding::claude_auth::CliAuthManager::new()),
        audit_tx,
        webhook_semaphore: Arc::new(tokio::sync::Semaphore::new(50)),
        mesh_ca: None,
    };

    // Match production behavior: initialize permission cache TTL
    platform::rbac::resolver::set_cache_ttl(config.permission_cache_ttl_secs);

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
/// Includes the main API router plus observe (query + alerts), git protocol,
/// and registry routers. The observe ingest routes (OTLP) are omitted since
/// they require background channels.
pub fn test_router(state: AppState) -> Router {
    use axum::extract::DefaultBodyLimit;
    use tower_http::limit::RequestBodyLimitLayer;
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
        .merge(platform::api::preview::router())
        .merge(platform::observe::query::router())
        .merge(platform::observe::alert::router())
        // Git protocol + registry routes need a higher body limit (500 MB).
        // Both RequestBodyLimitLayer AND DefaultBodyLimit must be set because
        // axum's Bytes extractor wraps the body in an *additional* Limited
        // based on DefaultBodyLimit (see axum-core with_limited_body()).
        .merge(
            platform::git::git_protocol_router()
                .layer(DefaultBodyLimit::disable())
                .layer(RequestBodyLimitLayer::new(500 * 1024 * 1024)),
        )
        .merge(
            platform::registry::router()
                .layer(DefaultBodyLimit::disable())
                .layer(RequestBodyLimitLayer::new(500 * 1024 * 1024)),
        )
        .with_state(state)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
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

/// Poll the `audit_log` table until at least one entry with the given action appears,
/// or timeout after `max_ms` milliseconds. Returns the count found.
///
/// Audit entries are written asynchronously via `tokio::spawn` in `send_audit()`,
/// so tests that query `audit_log` immediately after an API call may see zero rows.
/// This helper avoids that race by polling with short sleeps.
pub async fn wait_for_audit(pool: &PgPool, action: &str, max_ms: u64) -> i64 {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(max_ms);
    loop {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log WHERE action = $1")
            .bind(action)
            .fetch_one(pool)
            .await
            .unwrap();
        if count > 0 {
            return count;
        }
        if tokio::time::Instant::now() > deadline {
            return 0;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
    }
}

/// Guard that aborts a spawned server task when dropped, ensuring the TCP port
/// is released for the next test.
pub struct ServerGuard(tokio::task::AbortHandle);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Start a real TCP server for integration tests that need pod connectivity.
///
/// Binds to `PLATFORM_LISTEN_PORT` (set by `test-in-cluster.sh`).
/// Returns `(state, admin_token, server_guard)`. The guard aborts the server
/// task on drop so the port is released for subsequent tests.
pub async fn start_test_server(pool: PgPool) -> (AppState, String, ServerGuard) {
    let port: u16 = std::env::var("PLATFORM_LISTEN_PORT")
        .expect("PLATFORM_LISTEN_PORT must be set — run via: just test-integration")
        .parse()
        .expect("invalid PLATFORM_LISTEN_PORT");
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("parse listen addr");
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
        .expect("create socket");
    socket.set_reuse_address(true).expect("set SO_REUSEADDR");
    socket.bind(&addr.into()).expect("bind listener");
    socket.listen(128).expect("listen");
    socket.set_nonblocking(true).expect("set nonblocking");
    let listener =
        tokio::net::TcpListener::from_std(socket.into()).expect("convert to tokio listener");
    let (state, token) = test_state(pool).await;
    let app = test_router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, token, ServerGuard(handle.abort_handle()))
}

/// Start a real TCP server for pipeline executor tests.
///
/// Binds to `PLATFORM_LISTEN_PORT` (same as `start_test_server`) so that K8s
/// pods in Kind can reach the host via the address configured by the test script.
/// **Must be serialized** via nextest test-groups to avoid port conflicts — see
/// `.config/nextest.toml` `serial-listen-port` group.
///
/// Returns `(state, admin_token, server_guard)`.
pub async fn start_pipeline_server(pool: PgPool) -> (AppState, String, ServerGuard) {
    start_test_server(pool).await
}

/// Extract JSON body from a response.
async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body is not valid JSON")
}

// ---------------------------------------------------------------------------
// Raw bytes HTTP helpers
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

/// Send a raw GET request and return only the status code (for non-JSON endpoints
/// like proxy responses where the body format is unknown).
pub async fn get_status(app: &Router, token: &str, path: &str) -> StatusCode {
    let mut builder = Request::builder().method("GET").uri(path);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    resp.status()
}

/// Send a POST request with JSON body and return only the status code (for endpoints
/// that may return non-JSON responses like axum deserialization rejections).
pub async fn post_status(app: &Router, token: &str, path: &str, body: Value) -> StatusCode {
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
    resp.status()
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

// ---------------------------------------------------------------------------
// Merge-gate test helpers
// ---------------------------------------------------------------------------

/// Get the admin user's UUID from the DB.
pub async fn admin_user_id(pool: &PgPool) -> Uuid {
    let row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Insert a merge request directly in the DB (bypasses git/API).
/// Returns the MR's UUID.
pub async fn insert_mr(
    pool: &PgPool,
    project_id: Uuid,
    author_id: Uuid,
    source_branch: &str,
    target_branch: &str,
    number: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status, head_sha)
         VALUES ($1, $2, $3, $4, $5, $6, 'Test MR', 'open', 'abc123')",
    )
    .bind(id)
    .bind(project_id)
    .bind(number)
    .bind(author_id)
    .bind(source_branch)
    .bind(target_branch)
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

/// Insert a branch protection rule directly in the DB.
/// Returns the rule's UUID.
#[allow(clippy::too_many_arguments)]
pub async fn insert_branch_protection(
    pool: &PgPool,
    project_id: Uuid,
    pattern: &str,
    required_approvals: i32,
    merge_methods: &[&str],
    required_checks: &[&str],
    require_up_to_date: bool,
    allow_admin_bypass: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO branch_protection_rules
         (id, project_id, pattern, required_approvals, merge_methods, required_checks, require_up_to_date, allow_admin_bypass)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (project_id, pattern) DO UPDATE SET
           required_approvals = EXCLUDED.required_approvals,
           merge_methods = EXCLUDED.merge_methods,
           required_checks = EXCLUDED.required_checks,
           require_up_to_date = EXCLUDED.require_up_to_date,
           allow_admin_bypass = EXCLUDED.allow_admin_bypass
         RETURNING id",
    )
    .bind(id)
    .bind(project_id)
    .bind(pattern)
    .bind(required_approvals)
    .bind(merge_methods)
    .bind(required_checks)
    .bind(require_up_to_date)
    .bind(allow_admin_bypass)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

/// Insert a pipeline row directly in the DB. Returns the pipeline's UUID.
pub async fn insert_pipeline(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    status: &str,
    git_ref: &str,
    trigger: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, triggered_by, status, git_ref, trigger, commit_sha)
         VALUES ($1, $2, $3, $4, $5, $6, 'abc123')",
    )
    .bind(id)
    .bind(project_id)
    .bind(user_id)
    .bind(status)
    .bind(git_ref)
    .bind(trigger)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Poll a pipeline's status until it reaches a terminal state (success/failure/cancelled).
/// Returns the final status string. Panics if timeout is exceeded.
pub async fn poll_pipeline_status(
    app: &Router,
    token: &str,
    project_id: Uuid,
    pipeline_id: &str,
    timeout_secs: u64,
) -> String {
    let start = std::time::Instant::now();
    loop {
        let (_, body) = get_json(
            app,
            token,
            &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
        )
        .await;
        let status = body["status"].as_str().unwrap_or("unknown").to_string();
        if matches!(status.as_str(), "success" | "failure" | "cancelled") {
            return status;
        }
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "pipeline did not complete within {timeout_secs}s, last status: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
