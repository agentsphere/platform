pub mod bootstrap;
pub mod eventbus;
pub mod pool;
pub mod valkey;

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::agent::inprocess::InProcessHandle;
use crate::config::Config;
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
    /// In-process agent sessions (global/create-app sessions, not K8s pods).
    pub inprocess_sessions: Arc<std::sync::RwLock<HashMap<Uuid, InProcessHandle>>>,
    /// Ephemeral in-memory state for agent secret requests (5-min TTL).
    pub secret_requests: SecretRequests,
}
