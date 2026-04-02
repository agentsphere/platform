// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Application state, bootstrap, and connection pools.

pub mod bootstrap;
pub mod commands_seed;
pub mod eventbus;
pub mod pool;
pub mod valkey;

use std::sync::Arc;

use sqlx::PgPool;

use crate::agent::claude_cli::CliSessionManager;
use crate::config::Config;
use crate::health::{HealthSnapshot, TaskRegistry};
use crate::onboarding::claude_auth::CliAuthManager;
use crate::secrets::request::SecretRequests;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub kube: kube::Client,
    pub config: Arc<Config>,
    pub webauthn: Arc<webauthn_rs::prelude::Webauthn>,
    /// Notify the pipeline executor that a new pipeline is ready.
    pub pipeline_notify: Arc<tokio::sync::Notify>,
    /// Notify the deployer reconciler that a deployment is ready.
    pub deploy_notify: Arc<tokio::sync::Notify>,
    /// Ephemeral in-memory state for agent secret requests (5-min TTL).
    pub secret_requests: SecretRequests,
    /// CLI subprocess sessions running inside the platform pod.
    pub cli_sessions: CliSessionManager,
    /// Cached health snapshot, updated by the background health loop.
    pub health: Arc<std::sync::RwLock<HealthSnapshot>>,
    /// In-memory heartbeat tracker for background tasks.
    pub task_registry: Arc<TaskRegistry>,
    /// Manages active Claude CLI auth sessions for onboarding.
    pub cli_auth_manager: Arc<CliAuthManager>,
    /// Fire-and-forget audit log handle.
    pub audit_tx: crate::audit::AuditLog,
    /// Concurrency limiter for webhook dispatch (max 50 concurrent deliveries).
    pub webhook_semaphore: Arc<tokio::sync::Semaphore>,
    /// Mesh certificate authority (None if mesh is disabled).
    pub mesh_ca: Option<Arc<crate::mesh::MeshCa>>,
}
