pub mod bootstrap;
pub mod eventbus;
pub mod pool;
pub mod valkey;

use std::sync::Arc;

use sqlx::PgPool;

use crate::agent::claude_cli::CliSessionManager;
use crate::config::Config;
use crate::health::{HealthSnapshot, TaskRegistry};
use crate::secrets::request::SecretRequests;

#[derive(Clone)]
#[allow(dead_code)] // minio, kube, config consumed by modules 03-09
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
}
