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

use platform::config::Config;
use platform::store::AppState;

// ---------------------------------------------------------------------------
// AppState builders
// ---------------------------------------------------------------------------

/// Build a full E2E `AppState` with real K8s, MinIO, Valkey, and Postgres.
///
/// This is similar to the integration test `test_state` but connects to real
/// external services rather than using stubs. Falls back gracefully when
/// services are unavailable (tests should be `#[ignore]`).
pub async fn e2e_state(pool: PgPool) -> AppState {
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

    // Real MinIO via S3 operator
    let minio_endpoint =
        std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let minio_access =
        std::env::var("MINIO_ACCESS_KEY").unwrap_or_else(|_| "platform".into());
    let minio_secret =
        std::env::var("MINIO_SECRET_KEY").unwrap_or_else(|_| "devdevdev".into());

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

    // Real Kube client (from KUBECONFIG or in-cluster)
    let kube = kube::Client::try_default()
        .await
        .unwrap_or_else(|_| {
            // No kubeconfig available — build a stub that panics on use
            let cfg = kube::Config::new("https://127.0.0.1:1".parse().unwrap());
            kube::Client::try_from(cfg).expect("dummy kube client")
        });

    let git_repos_path = std::env::temp_dir().join(format!("platform-e2e-repos-{}", Uuid::new_v4()));
    let ops_repos_path = std::env::temp_dir().join(format!("platform-e2e-ops-{}", Uuid::new_v4()));

    let config = Config {
        listen: "127.0.0.1:0".into(),
        database_url: "postgres://localhost/test".into(),
        valkey_url,
        minio_endpoint,
        minio_access_key: minio_access,
        minio_secret_key: minio_secret,
        master_key: None,
        git_repos_path,
        ops_repos_path,
        smtp_host: None,
        smtp_port: 587,
        smtp_from: "test@localhost".into(),
        smtp_username: None,
        smtp_password: None,
        admin_password: None,
        pipeline_namespace: "e2e-pipelines".into(),
        agent_namespace: "e2e-agents".into(),
        registry_url: None,
        secure_cookies: false,
        cors_origins: vec![],
        trust_proxy_headers: false,
        dev_mode: true,
        webauthn_rp_id: "localhost".into(),
        webauthn_rp_origin: "http://localhost:8080".into(),
        webauthn_rp_name: "Test Platform".into(),
    };

    let webauthn =
        platform::auth::passkey::build_webauthn(&config).expect("webauthn build failed");

    AppState {
        pool,
        valkey,
        minio,
        kube,
        config: Arc::new(config),
        webauthn: Arc::new(webauthn),
    }
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
pub fn create_bare_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let work_path = dir.path().join("work");
    git_cmd_at(
        dir.path(),
        &["clone", bare_path.to_str().unwrap(), "work"],
    );

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
        if start.elapsed().as_secs() > timeout_secs {
            panic!("pod {name} did not complete within {timeout_secs}s");
        }
        if let Ok(pod) = pods.get(name).await {
            if let Some(status) = &pod.status {
                if let Some(phase) = &status.phase {
                    if matches!(phase.as_str(), "Succeeded" | "Failed") {
                        return phase.clone();
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Cleanup K8s resources by label selector.
pub async fn cleanup_k8s(kube: &kube::Client, namespace: &str, label: &str) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::ListParams;
    use kube::Api;

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let lp = ListParams::default().labels(label);
    if let Ok(list) = pods.list(&lp).await {
        for pod in list.items {
            if let Some(name) = pod.metadata.name {
                let _ = pods.delete(&name, &Default::default()).await;
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
        let status = body["status"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        if matches!(status.as_str(), "success" | "failure" | "cancelled") {
            return status;
        }
        if start.elapsed().as_secs() > timeout_secs {
            panic!(
                "pipeline did not complete within {timeout_secs}s, last status: {status}"
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Poll a deployment's current_status until it matches the expected value.
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
        if status == "failed" {
            panic!("deployment reached failed status");
        }
        if start.elapsed().as_secs() > timeout_secs {
            panic!(
                "deployment did not reach '{expected}' within {timeout_secs}s, last: {status}"
            );
        }
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
pub async fn post_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
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
pub async fn patch_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
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
