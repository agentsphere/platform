// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Standalone OTLP ingest binary.
//!
//! Receives traces, logs, and metrics over HTTP protobuf, flushes to Postgres,
//! and publishes live-tail events to Valkey. Runs independently of the main
//! platform binary so telemetry keeps flowing during deploys/restarts.

use clap::Parser;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use platform_ingest::config::IngestConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default rustls crypto provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .json()
        .init();

    let cfg = IngestConfig::parse();

    let pool = platform_types::pool::pg_connect(&cfg.database_url, 10, 30).await?;
    let valkey = platform_types::valkey::connect(&cfg.valkey_url, 4).await?;

    platform_auth::resolver::set_cache_ttl(cfg.permission_cache_ttl);

    let cancel = CancellationToken::new();

    // Shut down on SIGINT
    let cancel_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received SIGINT, shutting down");
            cancel_signal.cancel();
        }
    });

    tracing::info!(listen = %cfg.listen, "starting platform-ingest");
    let listener = TcpListener::bind(&cfg.listen).await?;

    platform_ingest::run(
        listener,
        pool,
        valkey,
        cancel,
        cfg.trust_proxy,
        cfg.buffer_capacity,
    )
    .await?;

    tracing::info!("platform-ingest shutdown complete");
    Ok(())
}
