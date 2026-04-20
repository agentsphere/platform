// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! E2E test helpers — state construction, router builders, HTTP helpers.

use std::collections::HashMap;
use std::sync::{Arc, Once};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use fred::interfaces::ClientLike;
use platform_next::config::PlatformConfig;
use platform_next::state::PlatformState;
use platform_observe::ingest::IngestChannels;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

static INIT_TRACING: Once = Once::new();

fn init_test_tracing() {
    use tracing_subscriber::EnvFilter;

    INIT_TRACING.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        if let Ok(log_file) = std::env::var("TEST_LOG_FILE") {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
                .expect("failed to open TEST_LOG_FILE");
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::sync::Mutex::new(file))
                .with_thread_names(true)
                .json()
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
    });
}

// ---------------------------------------------------------------------------
// Kube config resolution
// ---------------------------------------------------------------------------

async fn resolve_kube_client() -> kube::Client {
    // kube::Client::try_default reads KUBECONFIG env var, then falls back
    // to in-cluster config. The Kind cluster sets KUBECONFIG appropriately.
    kube::Client::try_default()
        .await
        .expect("failed to create kube client — is KUBECONFIG set?")
}

// ---------------------------------------------------------------------------
// State construction
// ---------------------------------------------------------------------------

/// Construct a full `PlatformState` for E2E tests.
///
/// Runs bootstrap (seeds permissions, roles, admin user), connects to real
/// Valkey/MinIO/K8s, and returns `(state, admin_token)`.
#[allow(clippy::too_many_lines)]
pub async fn e2e_state(pool: PgPool) -> (PlatformState, String) {
    e2e_state_with_api_url(pool, None).await
}

/// Construct state with a custom `PLATFORM_API_URL` override (for TCP server tests).
#[allow(clippy::too_many_lines)]
pub async fn e2e_state_with_api_url(
    pool: PgPool,
    platform_api_url: Option<String>,
) -> (PlatformState, String) {
    init_test_tracing();

    // Ensure a rustls CryptoProvider is installed (needed by reqwest/fred)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Bootstrap: seed permissions, roles, admin user
    platform_next::bootstrap::run(&pool, Some("testpassword"), true)
        .await
        .expect("bootstrap failed");

    // -- Valkey --
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey_config =
        fred::types::config::Config::from_url(&valkey_url).expect("invalid VALKEY_URL");
    let valkey_pool = fred::clients::Pool::new(valkey_config, None, None, None, 1)
        .expect("valkey pool creation failed");
    valkey_pool.init().await.expect("valkey init failed");

    // -- MinIO --
    let minio_endpoint =
        std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let minio_access = std::env::var("MINIO_ACCESS_KEY").unwrap_or_else(|_| "platform".into());
    let minio_secret = std::env::var("MINIO_SECRET_KEY").unwrap_or_else(|_| "devdevdev".into());
    let minio_insecure = std::env::var("MINIO_INSECURE").ok().as_deref() == Some("true");

    let minio = {
        let builder = opendal::services::S3::default()
            .endpoint(&minio_endpoint)
            .access_key_id(&minio_access)
            .secret_access_key(&minio_secret)
            .bucket("platform-e2e")
            .region("us-east-1");
        match opendal::Operator::new(builder) {
            Ok(op) => {
                let op = op.finish();
                // Accept self-signed TLS certs for MinIO in dev/test
                if minio_insecure {
                    let client = reqwest::Client::builder()
                        .danger_accept_invalid_certs(true)
                        .build()
                        .expect("insecure reqwest client");
                    op.layer(opendal::layers::HttpClientLayer::new(
                        opendal::raw::HttpClient::with(client),
                    ))
                } else {
                    op
                }
            }
            Err(_) => {
                tracing::warn!("MinIO unavailable, using in-memory storage");
                opendal::Operator::new(opendal::services::Memory::default())
                    .expect("memory operator")
                    .finish()
            }
        }
    };

    // -- Kube --
    let kube = resolve_kube_client().await;

    // -- Config --
    let mut config = PlatformConfig::load();
    config.core.dev_mode = true;
    config.valkey.valkey_pool_size = 1;
    if let Some(url) = platform_api_url {
        config.pipeline.platform_api_url = url.clone();
        // Update registry URL to match the test server's actual port — the
        // env-var port (PLATFORM_REGISTRY_URL) may differ from the port 0
        // random binding used by start_*_server().
        let host_port = url.strip_prefix("http://").unwrap_or(&url);
        config.registry.registry_url = Some(host_port.to_string());
    }

    // Each parallel test instance needs its own repos directories to avoid
    // races on the shared filesystem (all demo tests create repos at the same
    // owner/project path).
    let test_suffix = &Uuid::new_v4().to_string()[..8];
    config.git.git_repos_path = config.git.git_repos_path.join(test_suffix);
    config.deployer.ops_repos_path = config.deployer.ops_repos_path.join(test_suffix);

    // -- WebAuthn --
    let webauthn = {
        let rp_id = &config.webauthn.webauthn_rp_id;
        let rp_origin = webauthn_rs::prelude::Url::parse(&config.webauthn.webauthn_rp_origin)
            .expect("invalid webauthn origin");
        let builder = webauthn_rs::WebauthnBuilder::new(rp_id, &rp_origin)
            .expect("webauthn builder")
            .rp_name(&config.webauthn.webauthn_rp_name);
        Arc::new(builder.build().expect("webauthn build"))
    };

    // -- Services --
    let secrets_resolver = config.secrets.master_key.as_deref().map(|hex| {
        let key = platform_secrets::parse_master_key(hex).expect("invalid master key");
        platform_next::services::AppSecretsResolver::new(pool.clone(), key)
    });

    let smtp_config = platform_next::services::to_notify_smtp_config(&config.smtp);
    let webhook_dispatch = platform_webhook::WebhookDispatch::new(pool.clone());
    let notification_dispatcher = platform_next::services::AppNotificationDispatcher::new(
        pool.clone(),
        valkey_pool.clone(),
        smtp_config,
        webhook_dispatch,
    );

    let cli_session_manager = platform_agent::claude_cli::session::CliSessionManager::new(
        config.agent.max_cli_subprocesses,
    );

    // -- Permission cache TTL --
    platform_auth::set_cache_ttl(config.auth.permission_cache_ttl_secs);

    let state = PlatformState {
        pool: pool.clone(),
        valkey: valkey_pool,
        minio,
        kube,
        config: Arc::new(config),
        pipeline_notify: Arc::new(tokio::sync::Notify::new()),
        deploy_notify: Arc::new(tokio::sync::Notify::new()),
        webauthn,
        task_registry: Arc::new(platform_types::health::TaskRegistry::new()),
        audit_tx: platform_types::AuditLog::new(pool.clone()),
        webhook_semaphore: Arc::new(tokio::sync::Semaphore::new(50)),
        mesh_ca: None,
        secrets_resolver,
        notification_dispatcher,
        health: Arc::new(std::sync::RwLock::new(
            platform_operator::health::HealthSnapshot::default(),
        )),
        secret_requests: Arc::new(std::sync::RwLock::new(HashMap::new())),
        cli_session_manager,
    };

    // -- Admin token --
    let admin_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .expect("admin user not found");

    let (raw_token, token_hash) = platform_auth::generate_api_token();
    let token_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at)
         VALUES ($1, $2, 'e2e-test', $3, now() + interval '1 hour')",
    )
    .bind(token_id)
    .bind(admin_id)
    .bind(&token_hash)
    .execute(&pool)
    .await
    .expect("failed to create admin test token");

    (state, raw_token)
}

// ---------------------------------------------------------------------------
// Router builders
// ---------------------------------------------------------------------------

/// Standard E2E test router (API + git + registry, no observe ingest).
pub fn test_router(state: PlatformState) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(platform_next::api::router())
        .merge(platform_next::registry::router())
        .with_state(state.clone())
        .merge(platform_next::git::router(&state))
}

/// Pipeline test router — includes git/registry for pod connectivity.
pub fn pipeline_test_router(state: PlatformState) -> Router {
    test_router(state)
}

/// Observe pipeline test router — includes OTLP ingest routes.
pub fn observe_pipeline_test_router(state: PlatformState, channels: IngestChannels) -> Router {
    let observe_state = state.observe_state();
    let observe_router = platform_observe::router(channels).with_state(observe_state);

    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(platform_next::api::router())
        .merge(platform_next::registry::router())
        .with_state(state.clone())
        .merge(platform_next::git::router(&state))
        .merge(observe_router)
}

// ---------------------------------------------------------------------------
// TCP server starters
// ---------------------------------------------------------------------------

fn host_addr_for_kind() -> String {
    if let Ok(pod_ip) = std::env::var("POD_IP") {
        return pod_ip;
    }
    if let Ok(addr) = std::env::var("E2E_HOST_ADDR") {
        return addr;
    }
    if cfg!(target_os = "macos") {
        "host.docker.internal".into()
    } else {
        "172.18.0.1".into()
    }
}

/// Bind to `PLATFORM_LISTEN_PORT` if set (in-cluster tests need a
/// deterministic port so the DaemonSet registry proxy can forward to us),
/// otherwise port 0 for local parallel safety.  Falls back to port 0 if the
/// configured port is already taken (parallel tests share the env var).
async fn bind_test_listener() -> tokio::net::TcpListener {
    let port = std::env::var("PLATFORM_LISTEN_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    if port > 0 {
        if let Ok(listener) = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await {
            return listener;
        }
        eprintln!("PLATFORM_LISTEN_PORT={port} already in use, falling back to random port");
    }
    tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("failed to bind test listener")
}

/// Start a real TCP server for pipeline pod connectivity tests.
pub async fn start_pipeline_server(
    pool: PgPool,
) -> (PlatformState, String, tokio::task::JoinHandle<()>) {
    let listener = bind_test_listener().await;
    let local_port = listener.local_addr().unwrap().port();

    let host = host_addr_for_kind();
    let api_url = format!("http://{host}:{local_port}");

    let (state, token) = e2e_state_with_api_url(pool, Some(api_url)).await;

    let app = pipeline_test_router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    (state, token, handle)
}

/// Start a real TCP server with observe ingest channels.
pub async fn start_observe_pipeline_server(
    pool: PgPool,
) -> (
    PlatformState,
    String,
    tokio::task::JoinHandle<()>,
    tokio_util::sync::CancellationToken,
) {
    let listener = bind_test_listener().await;
    let local_port = listener.local_addr().unwrap().port();

    let host = host_addr_for_kind();
    let api_url = format!("http://{host}:{local_port}");

    let (state, token) = e2e_state_with_api_url(pool, Some(api_url)).await;
    let observe_state = state.observe_state();

    let cancel = tokio_util::sync::CancellationToken::new();
    let tracker = tokio_util::task::TaskTracker::new();
    let channels =
        platform_observe::spawn_background_tasks(&observe_state, cancel.clone(), &tracker);
    let app = observe_pipeline_test_router(state.clone(), channels);

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    (state, token, handle, cancel)
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

pub async fn body_json(body: Body) -> Value {
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("failed to read body");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

pub async fn get_json(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp.into_body()).await;
    (status, body)
}

pub async fn post_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp.into_body()).await;
    (status, body)
}

pub async fn patch_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PATCH")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp.into_body()).await;
    (status, body)
}

pub async fn put_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp.into_body()).await;
    (status, body)
}

pub async fn delete_json(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp.into_body()).await;
    (status, body)
}

pub async fn post_protobuf(app: &Router, token: &str, path: &str, body: Vec<u8>) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/x-protobuf")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    resp.status()
}

// ---------------------------------------------------------------------------
// Polling helpers
// ---------------------------------------------------------------------------

/// Poll a pipeline until it reaches a terminal status.
pub async fn poll_pipeline_status(
    app: &Router,
    token: &str,
    project_id: Uuid,
    pipeline_id: &str,
    timeout_secs: u64,
) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let (status, body) = get_json(
            app,
            token,
            &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
        )
        .await;
        if status == StatusCode::OK {
            if let Some(s) = body["status"].as_str() {
                if matches!(s, "success" | "failure" | "cancelled") {
                    return s.to_string();
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "pipeline {pipeline_id} did not reach terminal status within {timeout_secs}s, last: {body}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Poll a deployment until it reaches the expected status.
pub async fn poll_deployment_status(
    app: &Router,
    token: &str,
    project_id: Uuid,
    env: &str,
    expected: &str,
    timeout_secs: u64,
) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let (status, body) = get_json(
            app,
            token,
            &format!("/api/projects/{project_id}/deployments?environment={env}"),
        )
        .await;
        if status == StatusCode::OK {
            if let Some(items) = body["items"].as_array() {
                if let Some(first) = items.first() {
                    let cs = first["current_status"].as_str().unwrap_or("");
                    assert_ne!(cs, "failed", "deployment failed unexpectedly: {first}");
                    if cs == expected {
                        return cs.to_string();
                    }
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "deployment for {env} did not reach '{expected}' within {timeout_secs}s, last: {body}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

// ---------------------------------------------------------------------------
// K8s helpers
// ---------------------------------------------------------------------------

/// Wait for a pod to reach a terminal phase.
pub async fn wait_for_pod(
    kube: &kube::Client,
    namespace: &str,
    name: &str,
    timeout_secs: u64,
) -> String {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        if let Ok(pod) = pods.get(name).await {
            if let Some(status) = &pod.status {
                if let Some(phase) = &status.phase {
                    if matches!(phase.as_str(), "Succeeded" | "Failed") {
                        return phase.clone();
                    }
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!("pod {namespace}/{name} did not complete within {timeout_secs}s");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Fetch pod logs for a specific container.
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
    .unwrap_or_default()
}

/// Fetch pod logs (default container).
pub async fn pod_logs(kube: &kube::Client, namespace: &str, pod_name: &str) -> String {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;
    use kube::api::LogParams;

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    pods.logs(pod_name, &LogParams::default())
        .await
        .unwrap_or_default()
}

/// Delete pods matching a label selector.
pub async fn cleanup_k8s(kube: &kube::Client, namespace: &str, label: &str) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;
    use kube::api::{DeleteParams, ListParams};

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let lp = ListParams::default().labels(label);
    if let Ok(list) = pods.list(&lp).await {
        for pod in list {
            if let Some(name) = pod.metadata.name {
                let _ = pods.delete(&name, &DeleteParams::default()).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Run a git command in the given directory.
pub fn git_cmd(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "E2E Test")
        .env("GIT_AUTHOR_EMAIL", "e2e@test.local")
        .env("GIT_COMMITTER_NAME", "E2E Test")
        .env("GIT_COMMITTER_EMAIL", "e2e@test.local")
        .output()
        .expect("git command failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
