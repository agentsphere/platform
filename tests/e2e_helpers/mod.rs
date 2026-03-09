#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
// Kube config helpers
// ---------------------------------------------------------------------------

/// Resolve candidate kubeconfig paths for Kind clusters.
/// Handles the case where KUBECONFIG env var contains a path with unexpanded
/// `$HOME` (e.g. `/.kube/kind-platform` instead of `/Users/x/.kube/kind-platform`).
fn resolve_kubeconfig_candidates() -> Vec<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut paths = Vec::new();

    // 1. Try the KUBECONFIG value as-is
    if let Ok(raw) = std::env::var("KUBECONFIG") {
        paths.push(PathBuf::from(&raw));
        // If it doesn't start with a valid dir, try prepending HOME
        if !raw.starts_with(&home) && !raw.is_empty() {
            paths.push(PathBuf::from(format!("{home}{raw}")));
        }
    }

    // 2. Kind platform config at well-known location
    if !home.is_empty() {
        paths.push(PathBuf::from(format!("{home}/.kube/kind-platform")));
        paths.push(PathBuf::from(format!("{home}/.kube/config")));
    }

    paths
}

// ---------------------------------------------------------------------------
// AppState builders
// ---------------------------------------------------------------------------

/// Build a full E2E `AppState` and pre-authenticated admin API token.
///
/// This is similar to the integration test `test_state` but connects to real
/// external services rather than using stubs. Falls back gracefully when
/// services are unavailable (tests should be `#[ignore]`).
///
/// Returns `(state, admin_token)`. The admin token bypasses the login
/// endpoint's rate limiter.
pub async fn e2e_state(pool: PgPool) -> (AppState, String) {
    e2e_state_with_api_url(pool, None).await
}

/// Build a full E2E `AppState` with a custom `platform_api_url`.
///
/// When `platform_api_url` is `None`, falls back to `PLATFORM_API_URL` env var
/// or the default in-cluster URL.
#[allow(clippy::too_many_lines)]
pub async fn e2e_state_with_api_url(
    pool: PgPool,
    platform_api_url: Option<String>,
) -> (AppState, String) {
    // Ensure a rustls CryptoProvider is installed (needed by reqwest/fred)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Bootstrap seed data
    platform::store::bootstrap::run(&pool, Some("testpassword"), true)
        .await
        .expect("bootstrap failed");

    // Connect to real Valkey — no FLUSHDB needed (see tests/helpers/mod.rs).
    // Pool size 1 (not 4) to reduce connection count under parallel tests.
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey_config =
        fred::types::config::Config::from_url(&valkey_url).expect("invalid VALKEY_URL");
    let valkey = fred::clients::Pool::new(valkey_config, None, None, None, 1)
        .expect("valkey pool creation failed");
    valkey.init().await.expect("valkey connection failed");

    // Real MinIO via S3 operator
    let minio_endpoint =
        std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let minio_access = std::env::var("MINIO_ACCESS_KEY").unwrap_or_else(|_| "platform".into());
    let minio_secret = std::env::var("MINIO_SECRET_KEY").unwrap_or_else(|_| "devdevdev".into());

    let minio = {
        let builder = opendal::services::S3::default()
            .endpoint(&minio_endpoint)
            .access_key_id(&minio_access)
            .secret_access_key(&minio_secret)
            .bucket("platform-e2e")
            .region("us-east-1");
        // Fall back to in-memory if MinIO is unavailable
        match opendal::Operator::new(builder) {
            Ok(op) => op.finish(),
            Err(_) => opendal::Operator::new(opendal::services::Memory::default())
                .expect("memory operator")
                .finish(),
        }
    };

    // Real Kube client (from KUBECONFIG or in-cluster).
    // kube::Client::try_default() reads KUBECONFIG env var, but shell variable
    // expansion ($HOME) may not work correctly in all test harnesses. If it
    // fails, try resolving known kubeconfig paths explicitly.
    let kube = if let Ok(client) = kube::Client::try_default().await {
        client
    } else {
        // try_default failed — attempt to load kubeconfig from known paths
        let candidates = resolve_kubeconfig_candidates();
        let mut loaded = None;
        for path in &candidates {
            if let Ok(kubeconfig) = kube::config::Kubeconfig::read_from(path) {
                let opts = kube::config::KubeConfigOptions::default();
                if let Ok(kc) = kube::Config::from_custom_kubeconfig(kubeconfig, &opts).await
                    && let Ok(client) = kube::Client::try_from(kc)
                {
                    loaded = Some(client);
                    break;
                }
            }
        }
        loaded.unwrap_or_else(|| {
            panic!("[E2E] kube client unavailable — tried: {candidates:?}. Run `just cluster-up` first.");
        })
    };

    let git_repos_path =
        std::env::temp_dir().join(format!("platform-e2e-repos-{}", Uuid::new_v4()));
    let ops_repos_path = std::env::temp_dir().join(format!("platform-e2e-ops-{}", Uuid::new_v4()));

    let config = Config {
        listen: "127.0.0.1:0".into(),
        database_url: "postgres://localhost/test".into(),
        valkey_url,
        minio_endpoint,
        minio_access_key: minio_access,
        minio_secret_key: minio_secret,
        master_key: std::env::var("PLATFORM_MASTER_KEY").ok().or(Some(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        )),
        git_repos_path,
        ops_repos_path,
        smtp_host: None,
        smtp_port: 587,
        smtp_from: "test@localhost".into(),
        smtp_username: None,
        smtp_password: None,
        admin_password: None,
        pipeline_namespace: std::env::var("PLATFORM_PIPELINE_NAMESPACE")
            .expect("PLATFORM_PIPELINE_NAMESPACE must be set — run via: just test-e2e"),
        agent_namespace: std::env::var("PLATFORM_AGENT_NAMESPACE")
            .expect("PLATFORM_AGENT_NAMESPACE must be set — run via: just test-e2e"),
        registry_url: std::env::var("PLATFORM_REGISTRY_URL").ok(),
        secure_cookies: false,
        cors_origins: vec![],
        trust_proxy_headers: false,
        dev_mode: true,
        webauthn_rp_id: "localhost".into(),
        webauthn_rp_origin: "http://localhost:8080".into(),
        permission_cache_ttl_secs: 300,
        webauthn_rp_name: "Test Platform".into(),
        platform_api_url: platform_api_url.unwrap_or_else(|| {
            std::env::var("PLATFORM_API_URL")
                .unwrap_or_else(|_| "http://platform.platform.svc.cluster.local:8080".into())
        }),
        platform_namespace: "test-platform".into(),
        ssh_listen: None,
        ssh_host_key_path: "/tmp/test_ssh_host_key".into(),
        max_cli_subprocesses: 10,
        valkey_agent_host: std::env::var("PLATFORM_VALKEY_AGENT_HOST")
            .unwrap_or_else(|_| "localhost:6379".into()),
        agent_runner_dir: std::env::var("PLATFORM_AGENT_RUNNER_DIR").map_or_else(
            |_| std::env::temp_dir().join(format!("agent-runner-{}", uuid::Uuid::new_v4())),
            std::path::PathBuf::from,
        ),
        claude_cli_version: "stable".into(),
        ns_prefix: std::env::var("PLATFORM_NS_PREFIX").ok(),
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

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the full API router for E2E testing.
pub fn test_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(platform::api::router())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

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

/// Assign a role to a user.
pub async fn assign_role(
    app: &Router,
    admin_token: &str,
    user_id: Uuid,
    role_name: &str,
    project_id: Option<Uuid>,
    pool: &PgPool,
) {
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

// ---------------------------------------------------------------------------
// Git repo helpers
// ---------------------------------------------------------------------------

/// Create a bare git repo in a tempdir, return `(TempDir, PathBuf)`.
///
/// Uses `/tmp/platform-e2e/` as the base directory so that repos are visible
/// inside the Kind cluster (which has `/tmp/platform-e2e` as an extra mount).
pub fn create_bare_repo() -> (TempDir, PathBuf) {
    let base = std::path::Path::new("/tmp/platform-e2e");
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
    let base = std::path::Path::new("/tmp/platform-e2e");
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

/// Run a git command in a directory (alternative: no env override).
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
// K8s helpers
// ---------------------------------------------------------------------------

/// Wait for a K8s pod to reach a terminal phase (`Succeeded` or `Failed`).
///
/// Returns the phase string. Panics on timeout.
pub async fn wait_for_pod(
    kube: &kube::Client,
    namespace: &str,
    name: &str,
    timeout_secs: u64,
) -> String {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let start = std::time::Instant::now();
    loop {
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "pod {name} did not complete within {timeout_secs}s"
        );
        if let Ok(pod) = pods.get(name).await
            && let Some(status) = &pod.status
            && let Some(phase) = &status.phase
            && matches!(phase.as_str(), "Succeeded" | "Failed")
        {
            return phase.clone();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Fetch logs from a specific container in a K8s pod.
///
/// Returns the log text, or an error message if fetching fails.
pub async fn pod_logs_container(
    kube: &kube::Client,
    namespace: &str,
    pod_name: &str,
    container: &str,
) -> String {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;
    use kube::api::LogParams;

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    pods.logs(
        pod_name,
        &LogParams {
            container: Some(container.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_else(|e| format!("[log fetch failed: {e}]"))
}

/// Fetch logs from the main "claude" container in a K8s pod.
pub async fn pod_logs(kube: &kube::Client, namespace: &str, pod_name: &str) -> String {
    pod_logs_container(kube, namespace, pod_name, "claude").await
}

/// Cleanup K8s resources by label selector.
pub async fn cleanup_k8s(kube: &kube::Client, namespace: &str, label: &str) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;
    use kube::api::{DeleteParams, ListParams};

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let lp = ListParams::default().labels(label);
    if let Ok(list) = pods.list(&lp).await {
        for pod in list.items {
            if let Some(name) = pod.metadata.name {
                let _ = pods.delete(&name, &DeleteParams::default()).await;
            }
        }
    }
}

/// Poll a pipeline's status until it reaches a terminal state.
///
/// Returns the final status string (e.g. "success", "failure", "cancelled").
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
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Poll a deployment's `current_status` until it matches the expected value.
///
/// Returns the final status string.
pub async fn poll_deployment_status(
    app: &Router,
    token: &str,
    project_id: Uuid,
    env: &str,
    expected: &str,
    timeout_secs: u64,
) -> String {
    let start = std::time::Instant::now();
    loop {
        let (_, body) = get_json(
            app,
            token,
            &format!("/api/projects/{project_id}/deployments/{env}"),
        )
        .await;
        let status = body["current_status"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        if status == expected {
            return status;
        }
        assert!(status != "failed", "deployment reached failed status");
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "deployment did not reach '{expected}' within {timeout_secs}s, last: {status}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Pipeline E2E helpers — real HTTP server for git clone
// ---------------------------------------------------------------------------

/// Determine the hostname through which K8s pods can reach the test host.
///
/// Priority:
/// 1. `POD_IP` env var — set by Kubernetes downward API when running inside
///    a dev pod in k3s. The pod IP is routable from any other pod in the cluster.
/// 2. `E2E_HOST_ADDR` env var — explicit override.
/// 3. macOS: `host.docker.internal` (Docker Desktop bridge).
/// 4. Linux: `172.18.0.1` (Docker bridge gateway for Kind).
fn host_addr_for_kind() -> String {
    // In-cluster: pod IP set by Kubernetes downward API (hack/k3s/dev-env.yaml)
    if let Ok(pod_ip) = std::env::var("POD_IP") {
        return pod_ip;
    }
    // Explicit override
    if let Ok(addr) = std::env::var("E2E_HOST_ADDR") {
        return addr;
    }
    if cfg!(target_os = "macos") {
        "host.docker.internal".into()
    } else {
        "172.18.0.1".into()
    }
}

/// Build a full API + git-protocol router for pipeline E2E tests.
///
/// Unlike `test_router`, this includes the git smart HTTP routes so that
/// pipeline pods can clone repos via HTTP.
pub fn pipeline_test_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(platform::api::router())
        .merge(platform::git::git_protocol_router())
        .merge(platform::registry::router())
        .with_state(state)
}

/// Start a real TCP server for pipeline E2E tests.
///
/// Binds to `0.0.0.0:0`, starts serving the pipeline test router (with git
/// routes), and returns the `platform_api_url` reachable from Kind pods along
/// with the server handle and the port-bound state.
///
/// Usage:
/// ```ignore
/// let (state, token, _server) = e2e_helpers::start_pipeline_server(pool).await;
/// let app = e2e_helpers::pipeline_test_router(state.clone());
/// ```
pub async fn start_pipeline_server(
    pool: PgPool,
) -> (AppState, String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let host = host_addr_for_kind();
    let platform_api_url = format!("http://{host}:{port}");

    let (state, token) = e2e_state_with_api_url(pool, Some(platform_api_url)).await;
    let app = pipeline_test_router(state.clone());

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

/// Poll an agent session's status until it matches one of the expected values.
///
/// Queries the DB directly (no reaper dependency). Returns the final status.
pub async fn poll_session_status(
    pool: &PgPool,
    session_id: Uuid,
    expected: &[&str],
    timeout_secs: u64,
) -> String {
    let start = std::time::Instant::now();
    loop {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(pool)
                .await
                .unwrap();
        if let Some((status,)) = row
            && expected.contains(&status.as_str())
        {
            return status;
        }
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "session {session_id} did not reach expected status within {timeout_secs}s"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Start a real TCP server for agent E2E tests (with git routes for repo clone).
///
/// Binds to `PLATFORM_LISTEN_PORT` (set by `test-in-cluster.sh`) so that the
/// registry `DaemonSet` proxy can forward image pull requests to this server.
pub async fn start_agent_server(pool: PgPool) -> (AppState, String, tokio::task::JoinHandle<()>) {
    let port: u16 = std::env::var("PLATFORM_LISTEN_PORT")
        .expect("PLATFORM_LISTEN_PORT must be set — run via: just test-e2e")
        .parse()
        .expect("invalid PLATFORM_LISTEN_PORT");
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind listener");
    let host = host_addr_for_kind();
    let platform_api_url = format!("http://{host}:{port}");

    let (state, token) = e2e_state_with_api_url(pool, Some(platform_api_url)).await;
    let app = pipeline_test_router(state.clone());

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (state, token, handle)
}
