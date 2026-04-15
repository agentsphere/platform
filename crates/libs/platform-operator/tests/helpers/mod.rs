// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Shared test helpers for platform-operator integration tests.

use std::sync::Arc;

use platform_operator::health::HealthSnapshot;
use platform_operator::state::{OperatorConfig, OperatorState};
use platform_types::health::TaskRegistry;
use sqlx::PgPool;

pub async fn valkey_pool() -> fred::clients::Pool {
    let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let url = url.replace("redis://:", "redis://default:");
    let config = fred::types::config::Config::from_url(&url).expect("invalid VALKEY_URL");
    let pool =
        fred::clients::Pool::new(config, None, None, None, 2).expect("valkey pool creation failed");
    fred::interfaces::ClientLike::init(&pool)
        .await
        .expect("valkey connection failed");
    pool
}

pub fn minio_operator() -> opendal::Operator {
    let endpoint =
        std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let access_key = std::env::var("MINIO_ROOT_USER").unwrap_or_else(|_| "minioadmin".into());
    let secret_key = std::env::var("MINIO_ROOT_PASSWORD").unwrap_or_else(|_| "minioadmin".into());
    let bucket = std::env::var("MINIO_BUCKET").unwrap_or_else(|_| "platform-dev".into());

    let mut builder = opendal::services::S3::default();
    builder = builder.endpoint(&endpoint);
    builder = builder.bucket(&bucket);
    builder = builder.access_key_id(&access_key);
    builder = builder.secret_access_key(&secret_key);
    builder = builder.region("us-east-1");

    opendal::Operator::new(builder)
        .expect("minio operator build")
        .finish()
}

pub async fn kube_client() -> kube::Client {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    kube::Client::try_default()
        .await
        .expect("K8s client (is Kind cluster running?)")
}

pub async fn operator_state(pool: PgPool) -> OperatorState {
    OperatorState {
        pool,
        valkey: valkey_pool().await,
        kube: kube_client().await,
        minio: minio_operator(),
        config: Arc::new(OperatorConfig {
            health_check_interval_secs: 15,
            platform_namespace: "platform".into(),
            dev_mode: true,
            master_key: None,
            git_repos_path: "/tmp/git-repos".into(),
            registry_url: None,
            registry_node_url: None,
            gateway_name: "platform-gateway".into(),
            gateway_namespace: "platform".into(),
            gateway_auto_deploy: false,
            gateway_http_port: 8080,
            gateway_tls_port: 8443,
            gateway_http_node_port: 0,
            gateway_tls_node_port: 0,
            gateway_watch_namespaces: Vec::new(),
            platform_api_url: "http://platform.platform.svc.cluster.local:8080".into(),
        }),
        task_registry: Arc::new(TaskRegistry::new()),
        health: Arc::new(std::sync::RwLock::new(HealthSnapshot::default())),
    }
}
