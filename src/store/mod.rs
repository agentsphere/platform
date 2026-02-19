pub mod bootstrap;
pub mod pool;
pub mod valkey;

use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;

#[derive(Clone)]
#[allow(dead_code)] // minio, kube, config consumed by modules 03-09
pub struct AppState {
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub kube: kube::Client,
    pub config: Arc<Config>,
}
