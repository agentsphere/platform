use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue};
use tokio::signal;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod audit;
mod config;
mod error;
mod health;
mod store;
mod validation;

// Phase 02 — Identity, Auth & RBAC
mod api;
mod auth;
mod rbac;

// Phase 03 — Git Server
mod git;

// Phase 05 — Build Engine
mod pipeline;

// Phase 06 — Continuous Deployer
mod deployer;

// Phase 07 — Agent Orchestration
mod agent;

// Phase 08 — Observability
mod observe;

// Phase 10 — Web UI
mod ui;

// Phase 09 — Secrets Engine & Notifications
mod notify;
mod secrets;

// OCI Image Registry
mod registry;

// Workspaces
mod workspace;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    // Install rustls crypto provider before any TLS usage (kube, reqwest, lettre)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Platform self-observe: capture warn+ logs into the observe pipeline
    let self_observe_level = observe::tracing_layer::parse_level(
        &std::env::var("PLATFORM_SELF_OBSERVE_LEVEL").unwrap_or_else(|_| "warn".into()),
    );
    let (platform_logs_tx, platform_logs_rx) = observe::tracing_layer::create_channel();
    let platform_log_layer =
        observe::tracing_layer::PlatformLogLayer::new(platform_logs_tx, self_observe_level);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("PLATFORM_LOG").unwrap_or_else(|_| "info".into()))
        .with(fmt::layer().json())
        .with(platform_log_layer)
        .init();

    let mut cfg = config::Config::load();

    // Validate master key for secrets engine
    if let Some(ref mk) = cfg.master_key {
        secrets::engine::parse_master_key(mk).expect("PLATFORM_MASTER_KEY is invalid");
        tracing::info!("secrets engine master key loaded");
    } else if cfg.dev_mode {
        // Deterministic dev key — NOT for production
        let dev_key = "0".repeat(64);
        cfg.master_key = Some(dev_key);
        tracing::warn!(
            "PLATFORM_MASTER_KEY not set — using deterministic dev key (NOT for production)"
        );
    } else {
        tracing::warn!("PLATFORM_MASTER_KEY not set — secrets engine disabled");
    }

    // In dev mode, ensure data directories exist (use writable fallbacks if needed)
    if cfg.dev_mode {
        for dir in [&mut cfg.git_repos_path, &mut cfg.ops_repos_path] {
            if std::fs::create_dir_all(&*dir).is_err() {
                // Default /data/* paths aren't writable on dev machines — fall back to /tmp
                let fallback = std::env::temp_dir()
                    .join("platform-dev")
                    .join(dir.file_name().unwrap_or_default());
                std::fs::create_dir_all(&fallback).expect("failed to create dev data directory");
                tracing::warn!(
                    original = %dir.display(),
                    fallback = %fallback.display(),
                    "data directory not writable, using fallback"
                );
                *dir = fallback;
            }
        }
    }

    // Connect to Postgres and run migrations
    let pool = store::pool::connect(&cfg.database_url).await?;

    // Connect to Valkey
    let valkey = store::valkey::connect(&cfg.valkey_url).await?;

    // Create MinIO operator (opendal S3 backend)
    let minio = {
        let mut builder = opendal::services::S3::default();
        builder = builder
            .endpoint(&cfg.minio_endpoint)
            .access_key_id(&cfg.minio_access_key)
            .secret_access_key(&cfg.minio_secret_key)
            .bucket("platform")
            .region("us-east-1");
        opendal::Operator::new(builder)?.finish()
    };
    tracing::info!("minio operator created");

    // Create Kubernetes client
    let kube = kube::Client::try_default().await?;
    tracing::info!("kubernetes client created");

    // Initialize WebAuthn relying party
    let webauthn = auth::passkey::build_webauthn(&cfg)?;
    tracing::info!(rp_id = %cfg.webauthn_rp_id, "webauthn initialized");

    // Build AppState
    let state = store::AppState {
        pool: pool.clone(),
        valkey,
        minio,
        kube,
        config: Arc::new(cfg.clone()),
        webauthn: Arc::new(webauthn),
        pipeline_notify: Arc::new(tokio::sync::Notify::new()),
        deploy_notify: Arc::new(tokio::sync::Notify::new()),
        secret_requests: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        cli_sessions: agent::claude_cli::CliSessionManager::new(cfg.max_cli_subprocesses),
        health: Arc::new(std::sync::RwLock::new(health::HealthSnapshot::default())),
        task_registry: Arc::new(health::TaskRegistry::new()),
    };

    // Set configurable permission cache TTL
    rbac::resolver::set_cache_ttl(cfg.permission_cache_ttl_secs);

    // Bootstrap system roles, permissions, and create admin (dev) or setup token (prod)
    match store::bootstrap::run(&pool, cfg.admin_password.as_deref(), cfg.dev_mode).await? {
        store::bootstrap::BootstrapResult::Skipped => {}
        store::bootstrap::BootstrapResult::DevAdmin => {
            tracing::info!("dev mode: admin user created with default credentials");
        }
        store::bootstrap::BootstrapResult::SetupToken(token) => {
            tracing::warn!("=======================================================");
            tracing::warn!("  SETUP TOKEN (use within 1 hour):");
            tracing::warn!("  {token}");
            tracing::warn!("  Open /setup in your browser and enter this token");
            tracing::warn!("  to create the first admin user.");
            tracing::warn!("=======================================================");
        }
    }

    // Seed registry images from OCI layout tarballs (idempotent)
    if let Err(e) = registry::seed::seed_all(&pool, &state.minio, &cfg.seed_images_path).await {
        tracing::warn!(error = %e, "registry image seeding failed");
    }

    let (shutdown_tx, observe_channels) = spawn_background_tasks(&state, &pool);

    // Bridge platform tracing logs into the observe pipeline
    observe::tracing_layer::spawn_bridge(platform_logs_rx, observe_channels.logs_tx.clone());

    // Build router
    let ready_state = state.clone();
    let app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route(
            "/readyz",
            axum::routing::get(move || {
                let s = ready_state.clone();
                async move {
                    if health::checks::is_ready(&s).await {
                        (axum::http::StatusCode::OK, "ok")
                    } else {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
                    }
                }
            }),
        )
        .merge(api::router())
        .merge(observe::router(observe_channels))
        // Git routes get a higher body limit (500 MB for push/LFS)
        .merge(git::git_protocol_router().layer(RequestBodyLimitLayer::new(500 * 1024 * 1024)))
        // Registry routes get a higher body limit (500 MB for blob uploads)
        .merge(registry::router().layer(RequestBodyLimitLayer::new(500 * 1024 * 1024)))
        .with_state(state)
        .fallback(ui::static_handler)
        // Default body limit: 10 MB for API endpoints
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024))
        // Security response headers
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(build_cors_layer(&cfg));

    let addr: SocketAddr = cfg.listen.parse()?;
    tracing::info!(%addr, "starting platform");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Signal background tasks to stop
    let _ = shutdown_tx.send(());

    tracing::info!("platform stopped");
    Ok(())
}

fn spawn_background_tasks(
    state: &store::AppState,
    pool: &sqlx::PgPool,
) -> (
    tokio::sync::watch::Sender<()>,
    observe::ingest::IngestChannels,
) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    tokio::spawn(pipeline::executor::run(state.clone(), shutdown_rx.clone()));
    tokio::spawn(store::eventbus::run(state.clone(), shutdown_tx.subscribe()));
    tokio::spawn(deployer::reconciler::run(
        state.clone(),
        shutdown_tx.subscribe(),
    ));
    tokio::spawn(deployer::preview::run(
        state.clone(),
        shutdown_tx.subscribe(),
    ));
    tokio::spawn(agent::service::run_reaper(
        state.clone(),
        shutdown_tx.subscribe(),
    ));
    let observe_channels = observe::spawn_background_tasks(state.clone(), shutdown_tx.subscribe());
    tokio::spawn(registry::gc::run(state.clone(), shutdown_tx.subscribe()));
    if state.config.ssh_listen.is_some() {
        tokio::spawn(git::ssh_server::run(state.clone(), shutdown_tx.subscribe()));
    }
    tokio::spawn(run_session_cleanup(
        pool.clone(),
        state.secret_requests.clone(),
    ));
    tokio::spawn(health::checks::run(state.clone(), shutdown_tx.subscribe()));
    (shutdown_tx, observe_channels)
}

async fn run_session_cleanup(
    pool: sqlx::PgPool,
    secret_requests: crate::secrets::request::SecretRequests,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        interval.tick().await;
        if let Err(e) = sqlx::query("DELETE FROM auth_sessions WHERE expires_at < now()")
            .execute(&pool)
            .await
        {
            tracing::warn!(error = %e, "expired sessions cleanup failed");
        }
        if let Err(e) = sqlx::query(
            "DELETE FROM api_tokens WHERE expires_at IS NOT NULL AND expires_at < now()",
        )
        .execute(&pool)
        .await
        {
            tracing::warn!(error = %e, "expired tokens cleanup failed");
        }
        // Evict stale secret requests (completed/timed-out older than 10 minutes)
        let evict_threshold = std::time::Duration::from_secs(600);
        if let Ok(mut map) = secret_requests.write() {
            let before = map.len();
            map.retain(|_, r| r.created_at.elapsed() < evict_threshold);
            let evicted = before - map.len();
            if evicted > 0 {
                tracing::debug!(evicted, "evicted stale secret requests");
            }
        }
        tracing::debug!("expired sessions/tokens cleanup complete");
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

fn build_cors_layer(cfg: &config::Config) -> CorsLayer {
    let cors = CorsLayer::new()
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PATCH,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::ACCEPT,
            axum::http::header::COOKIE,
        ])
        .allow_credentials(true);

    if cfg.cors_origins.is_empty() {
        // No origins configured — deny cross-origin requests
        cors.allow_origin(AllowOrigin::exact(HeaderValue::from_static("null")))
    } else {
        let origins: Vec<HeaderValue> = cfg
            .cors_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        cors.allow_origin(origins)
    }
}
