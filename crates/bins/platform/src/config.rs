// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Master configuration composed from domain sub-configs.

use std::env;
use std::path::PathBuf;

use platform_types::config::{
    AgentConfig, AuthConfig, CoreConfig, DbConfig, DeployerConfig as DeployerSubConfig,
    GatewayConfig, GitConfig, MeshConfig, ObserveConfig, OperatorConfig, PipelineSubConfig,
    RegistryConfig as RegistrySubConfig, SecretsConfig, SmtpConfig, StorageConfig, ValkeyConfig,
    WebAuthnConfig, WebhookConfig,
};

/// Master configuration for the platform binary.
///
/// Composes all domain sub-configs. Loaded from environment variables.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct PlatformConfig {
    pub core: CoreConfig,
    pub db: DbConfig,
    pub valkey: ValkeyConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub webauthn: WebAuthnConfig,
    pub git: GitConfig,
    pub pipeline: PipelineSubConfig,
    pub agent: AgentConfig,
    pub deployer: DeployerSubConfig,
    pub observe: ObserveConfig,
    pub secrets: SecretsConfig,
    pub smtp: SmtpConfig,
    pub registry: RegistrySubConfig,
    pub mesh: MeshConfig,
    pub gateway: GatewayConfig,
    pub operator: OperatorConfig,
    pub webhook: WebhookConfig,
}

impl std::fmt::Debug for PlatformConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformConfig")
            .field("core", &self.core)
            .field("db", &"[REDACTED]")
            .field("valkey", &"[REDACTED]")
            .field("storage", &self.storage)
            .field("auth", &self.auth)
            .field("secrets", &self.secrets)
            .field("smtp", &self.smtp)
            .finish_non_exhaustive()
    }
}

fn parse_cors_origins(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extract host:port from a Redis URL, falling back to `"localhost:6379"`.
fn derive_valkey_host_port(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_owned();
            let port = u.port().unwrap_or(6379);
            Some(format!("{host}:{port}"))
        })
        .unwrap_or_else(|| "localhost:6379".into())
}

impl PlatformConfig {
    /// Load configuration from environment variables.
    #[allow(clippy::too_many_lines)]
    pub fn load() -> Self {
        let valkey_url = env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let valkey_agent_host = env::var("PLATFORM_VALKEY_AGENT_HOST")
            .unwrap_or_else(|_| derive_valkey_host_port(&valkey_url));
        let platform_namespace =
            env::var("PLATFORM_NAMESPACE").unwrap_or_else(|_| "platform".into());
        let dev_mode = env::var("PLATFORM_DEV").ok().is_some_and(|v| v == "true");
        let ns_prefix = env::var("PLATFORM_NS_PREFIX").ok();
        let registry_url = env::var("PLATFORM_REGISTRY_URL").ok();
        let registry_node_url = env::var("PLATFORM_REGISTRY_NODE_URL").ok();
        let gateway_namespace =
            env::var("PLATFORM_GATEWAY_NAMESPACE").unwrap_or_else(|_| platform_namespace.clone());
        let proxy_binary_path = env::var("PLATFORM_PROXY_PATH").ok();
        let master_key = env::var("PLATFORM_MASTER_KEY").ok();
        let ops_repos_path = env::var("PLATFORM_OPS_REPOS_PATH")
            .map_or_else(|_| PathBuf::from("/data/ops-repos"), PathBuf::from);
        let platform_api_url = env::var("PLATFORM_API_URL")
            .unwrap_or_else(|_| "http://platform.platform.svc.cluster.local:8080".into());

        Self {
            core: CoreConfig {
                listen: env::var("PLATFORM_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
                dev_mode,
                platform_namespace: platform_namespace.clone(),
                ns_prefix: ns_prefix.clone(),
                request_timeout_secs: env::var("PLATFORM_REQUEST_TIMEOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300),
            },
            db: DbConfig {
                database_url: env::var("DATABASE_URL").unwrap_or_else(|_| {
                    "postgres://platform:dev@localhost:5432/platform_dev".into()
                }),
                db_max_connections: env::var("PLATFORM_DB_MAX_CONNECTIONS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(20),
                db_acquire_timeout_secs: env::var("PLATFORM_DB_ACQUIRE_TIMEOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            },
            valkey: ValkeyConfig {
                valkey_url,
                valkey_pool_size: env::var("PLATFORM_VALKEY_POOL_SIZE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(6),
                valkey_agent_host,
            },
            storage: StorageConfig {
                minio_endpoint: env::var("MINIO_ENDPOINT")
                    .unwrap_or_else(|_| "http://localhost:9000".into()),
                minio_access_key: env::var("MINIO_ACCESS_KEY")
                    .unwrap_or_else(|_| "platform".into()),
                minio_secret_key: env::var("MINIO_SECRET_KEY")
                    .unwrap_or_else(|_| "devdevdev".into()),
                minio_insecure: env::var("MINIO_INSECURE").ok().is_some_and(|v| v == "true"),
            },
            auth: AuthConfig {
                secure_cookies: env::var("PLATFORM_SECURE_COOKIES")
                    .ok()
                    .is_some_and(|v| v == "true"),
                cors_origins: env::var("PLATFORM_CORS_ORIGINS")
                    .ok()
                    .map_or_else(Vec::new, |v| parse_cors_origins(&v)),
                trust_proxy_headers: env::var("PLATFORM_TRUST_PROXY")
                    .ok()
                    .is_some_and(|v| v == "true"),
                trust_proxy_cidrs: env::var("PLATFORM_TRUST_PROXY_CIDR")
                    .ok()
                    .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect())
                    .unwrap_or_default(),
                permission_cache_ttl_secs: env::var("PLATFORM_PERMISSION_CACHE_TTL")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300),
                token_max_expiry_days: env::var("PLATFORM_TOKEN_MAX_EXPIRY_DAYS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(365),
                admin_password: env::var("PLATFORM_ADMIN_PASSWORD").ok(),
            },
            webauthn: WebAuthnConfig {
                webauthn_rp_id: env::var("WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".into()),
                webauthn_rp_origin: env::var("WEBAUTHN_RP_ORIGIN")
                    .unwrap_or_else(|_| "http://localhost:8080".into()),
                webauthn_rp_name: env::var("WEBAUTHN_RP_NAME")
                    .unwrap_or_else(|_| "Platform".into()),
            },
            git: GitConfig {
                git_repos_path: env::var("PLATFORM_GIT_REPOS_PATH")
                    .map_or_else(|_| PathBuf::from("/data/repos"), PathBuf::from),
                ssh_listen: env::var("PLATFORM_SSH_LISTEN").ok(),
                ssh_host_key_path: env::var("PLATFORM_SSH_HOST_KEY_PATH")
                    .unwrap_or_else(|_| "/data/ssh_host_ed25519_key".into()),
                git_http_timeout_secs: env::var("PLATFORM_GIT_HTTP_TIMEOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(600),
                max_lfs_object_bytes: env::var("PLATFORM_MAX_LFS_OBJECT_BYTES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5_368_709_120),
            },
            pipeline: PipelineSubConfig {
                pipeline_namespace: env::var("PLATFORM_PIPELINE_NAMESPACE")
                    .unwrap_or_else(|_| "platform-pipelines".into()),
                pipeline_max_parallel: env::var("PLATFORM_PIPELINE_MAX_PARALLEL")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(4),
                pipeline_timeout_secs: env::var("PLATFORM_PIPELINE_TIMEOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(3600),
                runner_image: env::var("PLATFORM_RUNNER_IMAGE")
                    .unwrap_or_else(|_| "platform-runner:v1".into()),
                git_clone_image: env::var("PLATFORM_GIT_CLONE_IMAGE")
                    .unwrap_or_else(|_| "alpine/git:2.47.2".into()),
                kaniko_image: env::var("PLATFORM_KANIKO_IMAGE")
                    .unwrap_or_else(|_| "gcr.io/kaniko-project/executor:v1.23.2-debug".into()),
                platform_api_url: platform_api_url.clone(),
                max_artifact_file_bytes: env::var("PLATFORM_MAX_ARTIFACT_FILE_BYTES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50 * 1024 * 1024),
                max_artifact_total_bytes: env::var("PLATFORM_MAX_ARTIFACT_TOTAL_BYTES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(500 * 1024 * 1024),
            },
            agent: AgentConfig {
                agent_namespace: env::var("PLATFORM_AGENT_NAMESPACE")
                    .unwrap_or_else(|_| "platform-agents".into()),
                max_cli_subprocesses: env::var("PLATFORM_MAX_CLI_SUBPROCESSES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
                agent_runner_dir: env::var("PLATFORM_AGENT_RUNNER_DIR")
                    .map_or_else(|_| PathBuf::from("/data/agent-runner"), PathBuf::from),
                proxy_binary_dir: env::var("PLATFORM_PROXY_BINARY_DIR")
                    .map_or_else(|_| PathBuf::from("/data/platform-proxy"), PathBuf::from),
                mcp_servers_tarball: env::var("PLATFORM_MCP_SERVERS_TARBALL")
                    .map_or_else(|_| PathBuf::from("/data/mcp-servers.tar.gz"), PathBuf::from),
                mcp_servers_path: {
                    let p = env::var("PLATFORM_MCP_SERVERS_PATH")
                        .unwrap_or_else(|_| "mcp/servers".into());
                    let path = PathBuf::from(&p);
                    if path.is_absolute() {
                        p
                    } else {
                        env::current_dir()
                            .map(|cwd| cwd.join(&path).to_string_lossy().into_owned())
                            .unwrap_or(p)
                    }
                },
                claude_cli_version: env::var("PLATFORM_CLAUDE_CLI_VERSION")
                    .unwrap_or_else(|_| "stable".into()),
                cli_spawn_enabled: env::var("PLATFORM_CLI_SPAWN_ENABLED").ok().as_deref()
                    != Some("false"),
                session_idle_timeout_secs: env::var("PLATFORM_SESSION_IDLE_TIMEOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1800),
                manager_session_max_per_user: env::var("PLATFORM_MANAGER_SESSION_MAX")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            },
            deployer: DeployerSubConfig {
                ops_repos_path,
                preview_proxy_url: env::var("PLATFORM_PREVIEW_PROXY_URL").ok(),
            },
            observe: ObserveConfig {
                observe_retention_days: env::var("PLATFORM_OBSERVE_RETENTION_DAYS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30),
                observe_buffer_capacity: env::var("PLATFORM_OBSERVE_BUFFER_CAPACITY")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10_000),
                self_observe_level: env::var("PLATFORM_SELF_OBSERVE_LEVEL")
                    .unwrap_or_else(|_| "warn".into()),
                alert_max_window_secs: env::var("PLATFORM_ALERT_MAX_WINDOW_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(86_400),
            },
            secrets: SecretsConfig {
                master_key: master_key.clone(),
                master_key_previous: env::var("PLATFORM_MASTER_KEY_PREVIOUS").ok(),
            },
            smtp: SmtpConfig {
                smtp_host: env::var("PLATFORM_SMTP_HOST").ok(),
                smtp_port: env::var("PLATFORM_SMTP_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(587),
                smtp_from: env::var("PLATFORM_SMTP_FROM")
                    .unwrap_or_else(|_| "platform@localhost".into()),
                smtp_username: env::var("PLATFORM_SMTP_USERNAME").ok(),
                smtp_password: env::var("PLATFORM_SMTP_PASSWORD").ok(),
            },
            registry: RegistrySubConfig {
                registry_url: registry_url.clone(),
                registry_node_url: registry_node_url.clone(),
                registry_proxy_blobs: env::var("REGISTRY_PROXY_BLOBS")
                    .ok()
                    .is_some_and(|v| v == "true"),
                seed_images_path: env::var("PLATFORM_SEED_IMAGES_PATH")
                    .map_or_else(|_| PathBuf::from("/data/seed-images"), PathBuf::from),
                registry_http_body_limit_bytes: env::var("PLATFORM_REGISTRY_HTTP_BODY_LIMIT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(2 * 1024 * 1024 * 1024),
                registry_max_blob_size_bytes: env::var("PLATFORM_REGISTRY_MAX_BLOB_SIZE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5_368_709_120),
            },
            mesh: MeshConfig {
                mesh_enabled: env::var("PLATFORM_MESH_ENABLED")
                    .ok()
                    .is_some_and(|v| v == "true"),
                mesh_strict_mtls: env::var("PLATFORM_MESH_STRICT")
                    .ok()
                    .is_some_and(|v| v == "true" || v == "1"),
                mesh_ca_cert_ttl_secs: env::var("PLATFORM_MESH_CERT_TTL")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(3600),
                mesh_ca_root_ttl_days: env::var("PLATFORM_MESH_CA_ROOT_TTL_DAYS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(365),
                proxy_binary_path: proxy_binary_path.clone(),
            },
            gateway: GatewayConfig {
                gateway_name: env::var("PLATFORM_GATEWAY_NAME")
                    .unwrap_or_else(|_| "platform-gateway".into()),
                gateway_namespace: gateway_namespace.clone(),
                gateway_auto_deploy: env::var("PLATFORM_GATEWAY_AUTO_DEPLOY")
                    .ok()
                    .is_some_and(|v| v == "true"),
                gateway_http_port: env::var("PLATFORM_GATEWAY_HTTP_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(8080),
                gateway_tls_port: env::var("PLATFORM_GATEWAY_TLS_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(8443),
                gateway_http_node_port: env::var("PLATFORM_GATEWAY_HTTP_NODE_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                gateway_tls_node_port: env::var("PLATFORM_GATEWAY_TLS_NODE_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                gateway_watch_namespaces: env::var("PLATFORM_GATEWAY_WATCH_NAMESPACES")
                    .ok()
                    .map(|s| {
                        s.split(',')
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                acme_enabled: env::var("PLATFORM_ACME_ENABLED")
                    .ok()
                    .is_some_and(|v| v == "true"),
                acme_directory_url: env::var("PLATFORM_ACME_DIRECTORY_URL")
                    .unwrap_or_else(|_| "https://acme-v02.api.letsencrypt.org/directory".into()),
                acme_contact_email: env::var("PLATFORM_ACME_CONTACT_EMAIL").ok(),
            },
            operator: OperatorConfig {
                health_check_interval_secs: env::var("PLATFORM_HEALTH_CHECK_INTERVAL")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(15),
                seed_commands_path: env::var("PLATFORM_SEED_COMMANDS_PATH")
                    .map_or_else(|_| PathBuf::from("/data/seed-commands"), PathBuf::from),
            },
            webhook: WebhookConfig {
                webhook_max_concurrent: env::var("PLATFORM_WEBHOOK_MAX_CONCURRENT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50),
            },
        }
    }

    /// Validate configuration for production readiness.
    pub fn validate(&self) -> (Vec<String>, Vec<String>) {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        if self.core.dev_mode {
            warnings.push("PLATFORM_DEV=true — dev mode enabled. DO NOT use in production.".into());
        }

        if !self.core.dev_mode {
            if self.secrets.master_key.is_none() {
                errors.push(
                    "PLATFORM_MASTER_KEY is required in production. \
                     Set it to a 64-character hex string (32 bytes)."
                        .into(),
                );
            }
            if self.storage.minio_access_key == "platform"
                && self.storage.minio_secret_key == "devdevdev"
            {
                errors.push(
                    "MinIO credentials are still set to dev defaults \
                     (platform/devdevdev). Set MINIO_ACCESS_KEY and \
                     MINIO_SECRET_KEY to production values."
                        .into(),
                );
            }
            if !self.auth.secure_cookies {
                warnings.push(
                    "PLATFORM_SECURE_COOKIES=false — session cookies lack \
                     Secure flag. Set to true when behind HTTPS."
                        .into(),
                );
            }
            if self.auth.cors_origins.is_empty() {
                warnings.push(
                    "PLATFORM_CORS_ORIGINS is empty — all cross-origin \
                     requests will be denied."
                        .into(),
                );
            }
        }

        (warnings, errors)
    }

    /// Build domain-specific [`platform_agent::config::AgentConfig`] from env-level sub-configs.
    pub fn to_agent_config(&self) -> platform_agent::config::AgentConfig {
        platform_agent::config::AgentConfig {
            platform_api_url: self.pipeline.platform_api_url.clone(),
            registry_url: self.registry.registry_url.clone(),
            registry_node_url: self.registry.registry_node_url.clone(),
            valkey_agent_host: self.valkey.valkey_agent_host.clone(),
            claude_cli_version: self.agent.claude_cli_version.clone(),
            runner_image: self.pipeline.runner_image.clone(),
            git_clone_image: self.pipeline.git_clone_image.clone(),
            agent_namespace: self.agent.agent_namespace.clone(),
            platform_namespace: self.core.platform_namespace.clone(),
            gateway_namespace: self.gateway.gateway_namespace.clone(),
            ns_prefix: self.core.ns_prefix.clone(),
            dev_mode: self.core.dev_mode,
            session_idle_timeout_secs: self.agent.session_idle_timeout_secs,
            proxy_binary_path: self.mesh.proxy_binary_path.clone(),
            master_key: self.secrets.master_key.clone(),
            listen: self.core.listen.clone(),
            mcp_servers_path: self.agent.mcp_servers_path.clone(),
            manager_session_max_per_user: self.agent.manager_session_max_per_user,
            cli_spawn_enabled: self.agent.cli_spawn_enabled,
        }
    }

    /// Build domain-specific [`platform_git::GitServerConfig`] from env-level sub-configs.
    pub fn to_git_server_config(&self) -> platform_git::GitServerConfig {
        platform_git::GitServerConfig {
            repos_path: self.git.git_repos_path.clone(),
            ssh_host_key_path: Some(std::path::PathBuf::from(&self.git.ssh_host_key_path)),
            ssh_listen_addr: self.git.ssh_listen.clone(),
            git_http_timeout_secs: self.git.git_http_timeout_secs,
            max_lfs_object_bytes: self.git.max_lfs_object_bytes,
        }
    }

    /// Build domain-specific [`platform_pipeline::config::PipelineConfig`] from env-level sub-configs.
    pub fn to_pipeline_config(&self) -> platform_pipeline::config::PipelineConfig {
        platform_pipeline::config::PipelineConfig {
            kaniko_image: self.pipeline.kaniko_image.clone(),
            git_clone_image: self.pipeline.git_clone_image.clone(),
            platform_api_url: self.pipeline.platform_api_url.clone(),
            platform_namespace: self.core.platform_namespace.clone(),
            ns_prefix: self.core.ns_prefix.clone(),
            gateway_namespace: self.gateway.gateway_namespace.clone(),
            registry_url: self.registry.registry_url.clone(),
            node_registry_url: self.registry.registry_node_url.clone(),
            pipeline_timeout_secs: self.pipeline.pipeline_timeout_secs,
            pipeline_max_parallel: self.pipeline.pipeline_max_parallel,
            dev_mode: self.core.dev_mode,
            master_key: self.secrets.master_key.clone(),
            ops_repos_path: self.deployer.ops_repos_path.to_string_lossy().into_owned(),
            proxy_binary_path: self.mesh.proxy_binary_path.clone(),
            pipeline_namespace: self.pipeline.pipeline_namespace.clone(),
            max_artifact_file_bytes: self.pipeline.max_artifact_file_bytes,
            max_artifact_total_bytes: self.pipeline.max_artifact_total_bytes,
        }
    }

    /// Build domain-specific [`platform_operator::state::OperatorConfig`] from env-level sub-configs.
    pub fn to_operator_config(&self) -> platform_operator::state::OperatorConfig {
        platform_operator::state::OperatorConfig {
            health_check_interval_secs: self.operator.health_check_interval_secs,
            platform_namespace: self.core.platform_namespace.clone(),
            dev_mode: self.core.dev_mode,
            master_key: self.secrets.master_key.clone(),
            git_repos_path: self.git.git_repos_path.clone(),
            registry_url: self.registry.registry_url.clone(),
            registry_node_url: self.registry.registry_node_url.clone(),
            gateway_name: self.gateway.gateway_name.clone(),
            gateway_namespace: self.gateway.gateway_namespace.clone(),
            gateway_auto_deploy: self.gateway.gateway_auto_deploy,
            gateway_http_port: self.gateway.gateway_http_port,
            gateway_tls_port: self.gateway.gateway_tls_port,
            gateway_http_node_port: self.gateway.gateway_http_node_port,
            gateway_tls_node_port: self.gateway.gateway_tls_node_port,
            gateway_watch_namespaces: self.gateway.gateway_watch_namespaces.clone(),
            platform_api_url: self.pipeline.platform_api_url.clone(),
        }
    }

    /// Build domain-specific [`platform_deployer::state::DeployerConfig`] from env-level sub-configs.
    pub fn to_deployer_config(&self) -> platform_deployer::state::DeployerConfig {
        platform_deployer::state::DeployerConfig {
            ops_repos_path: self.deployer.ops_repos_path.to_string_lossy().into_owned(),
            platform_namespace: self.core.platform_namespace.clone(),
            ns_prefix: self.core.ns_prefix.clone(),
            dev_mode: self.core.dev_mode,
            gateway_namespace: self.gateway.gateway_namespace.clone(),
            preview_proxy_url: self.deployer.preview_proxy_url.clone(),
            registry_node_url: self.registry.registry_node_url.clone(),
            registry_url: self.registry.registry_url.clone(),
            proxy_binary_path: self.mesh.proxy_binary_path.clone(),
            mesh_enabled: self.mesh.mesh_enabled,
            mesh_strict_mtls: self.mesh.mesh_strict_mtls,
            platform_api_url: self.pipeline.platform_api_url.clone(),
            gateway_name: self.gateway.gateway_name.clone(),
            master_key: self.secrets.master_key.clone(),
        }
    }
}
