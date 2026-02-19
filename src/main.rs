use std::net::SocketAddr;
use std::sync::Arc;

use tokio::signal;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod audit;
mod config;
mod error;
mod store;

// Phase 02 — Identity, Auth & RBAC
mod api;
mod auth;
mod rbac;

// Phase 03 — Git Server
mod git;

// Module stubs — populated in later phases
mod pipeline {}
mod deployer {}
mod agent {}
mod observe {}
mod secrets {}
mod notify {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("PLATFORM_LOG").unwrap_or_else(|_| "info".into()))
        .with(fmt::layer().json())
        .init();

    let cfg = config::Config::load();

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

    // Build AppState
    let state = store::AppState {
        pool: pool.clone(),
        valkey,
        minio,
        kube,
        config: Arc::new(cfg.clone()),
    };

    // Bootstrap system roles, permissions, and admin user on first run
    store::bootstrap::run(&pool, cfg.admin_password.as_deref()).await?;

    // Build router
    let app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(api::router())
        .with_state(state);

    let addr: SocketAddr = cfg.listen.parse()?;
    tracing::info!(%addr, "starting platform");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("platform stopped");
    Ok(())
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
