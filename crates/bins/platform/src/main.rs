// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Platform binary — consumes workspace crate APIs.
//!
//! This binary composes all domain crates into a single HTTP server
//! with background tasks for pipeline execution, deployment reconciliation,
//! health monitoring, and event processing.

use std::sync::Arc;

use fred::interfaces::ClientLike;
use tracing_subscriber::EnvFilter;

use platform_next::config::PlatformConfig;
use platform_next::state::PlatformState;
use platform_next::{api, eventbus, git, registry, services, ui};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let config = load_config()?;
    let state = init_infrastructure(config).await?;

    // ── Background tasks ─────────────────────────────────────────────────
    let cancel = tokio_util::sync::CancellationToken::new();
    let tracker = tokio_util::task::TaskTracker::new();
    tokio::spawn(eventbus::run(state.clone(), cancel.clone()));

    // ── Pipeline executor ────────────────────────────────────────────────
    let pipeline_state = state.pipeline_state();
    tokio::spawn(platform_pipeline::executor::run(
        pipeline_state,
        cancel.clone(),
    ));

    // ── Deployer reconciler + analysis ──────────────────────────────────
    let deployer_state = state.deployer_state();
    tokio::spawn(platform_deployer::reconciler::run(
        deployer_state.clone(),
        cancel.clone(),
    ));
    tokio::spawn(platform_deployer::analysis::run(
        deployer_state,
        cancel.clone(),
    ));

    // ── SSH server (optional) ──────────────────────────────────────────
    if state.config.git.ssh_listen.is_some() {
        let git_state = state.git_state();
        let ssh_cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) =
                platform_git::ssh_server::run(git_state.svc, git_state.config, ssh_cancel).await
            {
                tracing::error!(error = %e, "SSH server error");
            }
        });
    }

    // ── Health monitor ─────────────────────────────────────────────────
    let operator_state = state.operator_state();
    tokio::spawn(platform_operator::health::run(
        operator_state,
        cancel.clone(),
    ));

    // ── Observe subsystem ────────────────────────────────────────────────
    let observe_state = state.observe_state();
    tokio::spawn(platform_observe::partitions::run(
        state.pool.clone(),
        cancel.clone(),
    ));
    let observe_channels =
        platform_observe::spawn_background_tasks(&observe_state, cancel.clone(), &tracker);
    let observe_router = platform_observe::router(observe_channels).with_state(observe_state);

    // ── Agent preview watcher ────────────────────────────────────────────
    let agent_state = state.agent_state();
    tokio::spawn(platform_agent::preview_watcher::run(
        agent_state,
        cancel.clone(),
    ));

    // ── HTTP server ──────────────────────────────────────────────────────
    let app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(api::router())
        .merge(registry::router())
        .fallback(ui::static_handler)
        .with_state(state.clone())
        .merge(git::router(&state))
        .merge(observe_router);

    let listener = tokio::net::TcpListener::bind(&state.config.core.listen).await?;
    tracing::info!(listen = %state.config.core.listen, "platform-next listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutting down");
            cancel.cancel();
        })
        .await?;

    Ok(())
}

fn load_config() -> anyhow::Result<PlatformConfig> {
    let config = PlatformConfig::load();
    let (warnings, errors) = config.validate();
    for w in &warnings {
        tracing::warn!("{w}");
    }
    if !errors.is_empty() {
        for e in &errors {
            tracing::error!("{e}");
        }
        anyhow::bail!(
            "configuration validation failed with {} error(s)",
            errors.len()
        );
    }
    tracing::info!("platform-next starting");
    tracing::debug!(?config, "loaded configuration");
    Ok(config)
}

#[allow(clippy::too_many_lines)]
async fn init_infrastructure(config: PlatformConfig) -> anyhow::Result<PlatformState> {
    // ── Database ─────────────────────────────────────────────────────────
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.db.db_max_connections)
        .acquire_timeout(std::time::Duration::from_secs(
            config.db.db_acquire_timeout_secs,
        ))
        .connect(&config.db.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to database: {e}"))?;
    tracing::info!("database connected");

    sqlx::migrate!("../../../migrations")
        .run(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;
    tracing::info!("migrations applied");

    // ── Valkey ────────────────────────────────────────────────────────────
    let valkey_config = fred::types::config::Config::from_url(&config.valkey.valkey_url)?;
    let valkey_pool = fred::clients::Pool::new(
        valkey_config,
        None,
        None,
        None,
        config.valkey.valkey_pool_size,
    )?;
    valkey_pool.init().await?;
    tracing::info!("valkey connected");

    // ── MinIO ─────────────────────────────────────────────────────────────
    let builder = opendal::services::S3::default()
        .endpoint(&config.storage.minio_endpoint)
        .access_key_id(&config.storage.minio_access_key)
        .secret_access_key(&config.storage.minio_secret_key)
        .bucket("platform")
        .region("us-east-1");
    let minio = {
        let op = opendal::Operator::new(builder)?.finish();
        if config.storage.minio_insecure {
            // Accept self-signed TLS certs (dev/test MinIO with generated certs)
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .map_err(|e| anyhow::anyhow!("insecure reqwest client: {e}"))?;
            op.layer(opendal::layers::HttpClientLayer::new(
                opendal::raw::HttpClient::with(client),
            ))
        } else {
            op
        }
    };
    tracing::info!("minio configured");

    // ── Kubernetes ────────────────────────────────────────────────────────
    let kube = kube::Client::try_default()
        .await
        .map_err(|e| anyhow::anyhow!("failed to create kube client: {e}"))?;
    tracing::info!("kubernetes client ready");

    // ── WebAuthn ──────────────────────────────────────────────────────────
    let webauthn = {
        let rp_id = &config.webauthn.webauthn_rp_id;
        let rp_origin = webauthn_rs::prelude::Url::parse(&config.webauthn.webauthn_rp_origin)?;
        let builder = webauthn_rs::WebauthnBuilder::new(rp_id, &rp_origin)?
            .rp_name(&config.webauthn.webauthn_rp_name);
        Arc::new(builder.build()?)
    };

    // ── Mesh CA ──────────────────────────────────────────────────────────
    let mesh_ca = if config.mesh.mesh_enabled {
        let mesh_cfg = platform_mesh::MeshConfig {
            master_key_hex: config.secrets.master_key.clone(),
            root_ttl_days: config.mesh.mesh_ca_root_ttl_days,
            cert_ttl_secs: config.mesh.mesh_ca_cert_ttl_secs,
        };
        match platform_mesh::MeshCa::init(&pool, &mesh_cfg).await {
            Ok(ca) => Some(Arc::new(ca)),
            Err(e) => {
                tracing::error!(error = %e, "failed to init mesh CA");
                None
            }
        }
    } else {
        None
    };

    // ── Secrets resolver (optional — requires master key) ─────────────
    let secrets_resolver = config
        .secrets
        .master_key
        .as_deref()
        .map(|hex| -> anyhow::Result<_> {
            let key = platform_secrets::parse_master_key(hex)?;
            Ok(services::AppSecretsResolver::new(pool.clone(), key))
        })
        .transpose()?;
    if secrets_resolver.is_some() {
        tracing::info!("secrets resolver ready");
    } else {
        tracing::warn!("PLATFORM_MASTER_KEY not set — secrets resolver disabled");
    }

    // ── Notification dispatcher ────────────────────────────────────────
    let smtp_config = services::to_notify_smtp_config(&config.smtp);
    let webhook_dispatch = platform_webhook::WebhookDispatch::new(pool.clone());
    let notification_dispatcher = services::AppNotificationDispatcher::new(
        pool.clone(),
        valkey_pool.clone(),
        smtp_config,
        webhook_dispatch,
    );
    tracing::info!("notification dispatcher ready");

    let cli_session_manager = platform_agent::claude_cli::session::CliSessionManager::new(
        config.agent.max_cli_subprocesses,
    );

    Ok(PlatformState {
        pool: pool.clone(),
        valkey: valkey_pool,
        minio,
        kube,
        config: Arc::new(config),
        pipeline_notify: Arc::new(tokio::sync::Notify::new()),
        deploy_notify: Arc::new(tokio::sync::Notify::new()),
        webauthn,
        task_registry: Arc::new(platform_types::health::TaskRegistry::new()),
        audit_tx: platform_types::AuditLog::new(pool),
        webhook_semaphore: Arc::new(tokio::sync::Semaphore::new(50)),
        mesh_ca,
        secrets_resolver,
        notification_dispatcher,
        health: Arc::new(std::sync::RwLock::new(
            platform_operator::health::HealthSnapshot::default(),
        )),
        secret_requests: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        cli_session_manager,
    })
}
