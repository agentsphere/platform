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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("PLATFORM_LOG").unwrap_or_else(|_| "info".into()))
        .with(fmt::layer().json())
        .init();

    let cfg = config::Config::load();

    // Validate master key for secrets engine
    if let Some(ref mk) = cfg.master_key {
        secrets::engine::parse_master_key(mk).expect("PLATFORM_MASTER_KEY is invalid");
        tracing::info!("secrets engine master key loaded");
    } else if cfg.dev_mode {
        tracing::warn!(
            "PLATFORM_MASTER_KEY not set — using deterministic dev key (NOT for production)"
        );
    } else {
        tracing::warn!("PLATFORM_MASTER_KEY not set — secrets engine disabled");
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
    };

    // Bootstrap system roles, permissions, and admin user on first run
    store::bootstrap::run(&pool, cfg.admin_password.as_deref()).await?;

    // Start background tasks
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    tokio::spawn(pipeline::executor::run(state.clone(), shutdown_rx.clone()));
    let deployer_shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(deployer::reconciler::run(
        state.clone(),
        deployer_shutdown_rx,
    ));
    let preview_shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(deployer::preview::run(
        state.clone(),
        preview_shutdown_rx,
    ));

    // Start agent session reaper background task
    let agent_shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(agent::service::run_reaper(state.clone(), agent_shutdown_rx));

    // Start observe background tasks (flush, rotation, alert evaluation)
    let observe_shutdown_rx = shutdown_tx.subscribe();
    let observe_channels = observe::spawn_background_tasks(state.clone(), observe_shutdown_rx);

    // Spawn expired session/token cleanup task (hourly)
    tokio::spawn(run_session_cleanup(pool.clone()));

    // Build router
    let app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(api::router())
        .merge(observe::router(observe_channels))
        // Git routes get a higher body limit (500 MB for push/LFS)
        .merge(git::git_protocol_router().layer(RequestBodyLimitLayer::new(500 * 1024 * 1024)))
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

async fn run_session_cleanup(pool: sqlx::PgPool) {
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
