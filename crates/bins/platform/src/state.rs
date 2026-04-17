// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Platform state composing all domain substates.

use std::collections::HashMap;
use std::sync::Arc;

use platform_agent::claude_cli::session::CliSessionManager;
use platform_types::AuditLog;
use platform_types::health::TaskRegistry;
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::PlatformConfig;
use crate::services::{AppNotificationDispatcher, AppRegistryCredentials, AppSecretsResolver};

/// Central state for the platform binary.
///
/// Holds shared infrastructure resources and a reference to config.
/// Domain-specific substates are constructed from these shared resources
/// when needed by API handlers and background tasks.
#[derive(Clone)]
pub struct PlatformState {
    // -- Shared infrastructure --
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub kube: kube::Client,
    pub config: Arc<PlatformConfig>,

    // -- Coordination signals --
    pub pipeline_notify: Arc<tokio::sync::Notify>,
    pub deploy_notify: Arc<tokio::sync::Notify>,

    // -- WebAuthn --
    pub webauthn: Arc<webauthn_rs::prelude::Webauthn>,

    // -- Background task tracking --
    pub task_registry: Arc<TaskRegistry>,

    // -- Audit --
    pub audit_tx: AuditLog,

    // -- Concurrency control --
    pub webhook_semaphore: Arc<tokio::sync::Semaphore>,

    // -- Mesh CA (optional) --
    pub mesh_ca: Option<Arc<platform_mesh::MeshCa>>,

    // -- Health --
    pub health: Arc<std::sync::RwLock<platform_operator::health::HealthSnapshot>>,

    // -- Domain services --
    pub secrets_resolver: Option<AppSecretsResolver>,
    pub notification_dispatcher: AppNotificationDispatcher,

    // -- Agent session state --
    pub secret_requests:
        Arc<std::sync::RwLock<HashMap<Uuid, crate::secrets_request::SecretRequest>>>,
    pub cli_session_manager: CliSessionManager,
}

/// Concrete pipeline services type used by the binary.
pub type AppPipelineServices = platform_pipeline::ConcretePipelineServices<
    platform_webhook::WebhookDispatch,
    platform_ops_repo::OpsRepoService,
    platform_deployer::DeployerService,
    AppRegistryCredentials,
>;

impl PlatformState {
    /// Construct a [`GitServerState`] from shared infrastructure.
    ///
    /// Cheap to call — all inner fields are `Arc`/`Clone`.
    pub fn git_state(
        &self,
    ) -> platform_git::GitServerState<crate::git_services::AppGitServerServices> {
        let svc = crate::git_services::AppGitServerServices::new(
            self.pool.clone(),
            self.valkey.clone(),
            self.minio.clone(),
            self.config.git.git_repos_path.clone(),
            self.config.deployer.ops_repos_path.clone(),
            self.audit_tx.clone(),
            self.config.git.max_lfs_object_bytes,
        );
        platform_git::GitServerState {
            svc: std::sync::Arc::new(svc),
            config: std::sync::Arc::new(self.config.to_git_server_config()),
        }
    }

    /// Construct a [`PipelineState`] from shared infrastructure.
    ///
    /// Cheap to call — all inner fields are `Arc`/`Clone`.
    pub fn pipeline_state(&self) -> platform_pipeline::PipelineState<AppPipelineServices> {
        let svc = platform_pipeline::ConcretePipelineServices::new(
            Arc::new(platform_webhook::WebhookDispatch::new(self.pool.clone())),
            Arc::new(platform_ops_repo::OpsRepoService::new(self.pool.clone())),
            Arc::new(platform_deployer::DeployerService),
            Arc::new(AppRegistryCredentials::new(
                self.pool.clone(),
                self.config.registry.registry_url.clone(),
            )),
        );
        platform_pipeline::PipelineState {
            pool: self.pool.clone(),
            kube: self.kube.clone(),
            valkey: self.valkey.clone(),
            minio: self.minio.clone(),
            config: self.config.to_pipeline_config(),
            pipeline_notify: self.pipeline_notify.clone(),
            task_heartbeat: self.task_registry.clone() as Arc<dyn platform_types::TaskHeartbeat>,
            services: svc,
        }
    }

    /// Construct a [`DeployerState`] from shared infrastructure.
    ///
    /// Cheap to call — all inner fields are `Arc`/`Clone`.
    pub fn deployer_state(
        &self,
    ) -> platform_deployer::DeployerState<crate::services::AppReconcilerServices> {
        let master_key = self
            .config
            .secrets
            .master_key
            .as_deref()
            .and_then(|s| platform_secrets::parse_master_key(s).ok());
        let svc = crate::services::AppReconcilerServices::new(self.pool.clone(), master_key);
        platform_deployer::DeployerState {
            pool: self.pool.clone(),
            valkey: self.valkey.clone(),
            kube: self.kube.clone(),
            minio: self.minio.clone(),
            config: self.config.to_deployer_config(),
            deploy_notify: self.deploy_notify.clone(),
            task_heartbeat: self.task_registry.clone() as Arc<dyn platform_types::TaskHeartbeat>,
            services: svc,
        }
    }

    /// Construct an [`ObserveState`] from shared infrastructure.
    ///
    /// Cheap to call — all inner fields are `Arc`/`Clone`.
    pub fn observe_state(&self) -> platform_observe::state::ObserveState {
        platform_observe::state::ObserveState {
            pool: self.pool.clone(),
            valkey: self.valkey.clone(),
            minio: self.minio.clone(),
            config: platform_observe::state::ObserveConfig {
                retention_days: self.config.observe.observe_retention_days,
                buffer_capacity: self.config.observe.observe_buffer_capacity,
                alert_max_window_secs: self.config.observe.alert_max_window_secs,
                trust_proxy: self.config.auth.trust_proxy_headers,
                ..Default::default()
            },
            alert_router: Arc::new(tokio::sync::RwLock::new(
                platform_observe::alert::AlertRouter::empty(),
            )),
        }
    }

    /// Construct an [`OperatorState`] from shared infrastructure.
    ///
    /// Cheap to call — all inner fields are `Arc`/`Clone`.
    pub fn operator_state(&self) -> platform_operator::state::OperatorState {
        platform_operator::state::OperatorState {
            pool: self.pool.clone(),
            valkey: self.valkey.clone(),
            kube: self.kube.clone(),
            minio: self.minio.clone(),
            config: Arc::new(self.config.to_operator_config()),
            task_registry: self.task_registry.clone(),
            health: self.health.clone(),
        }
    }

    /// Construct an [`AgentState`] from shared infrastructure.
    ///
    /// Cheap to call per-request — all fields are `Arc`/`Clone`.
    pub fn agent_state(&self) -> platform_agent::state::AgentState {
        platform_agent::state::AgentState {
            pool: self.pool.clone(),
            valkey: self.valkey.clone(),
            kube: self.kube.clone(),
            minio: self.minio.clone(),
            config: Arc::new(self.config.to_agent_config()),
            cli_sessions: self.cli_session_manager.clone(),
            task_registry: self.task_registry.clone(),
        }
    }
}
