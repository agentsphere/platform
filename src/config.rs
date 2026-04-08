// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::env;
use std::path::PathBuf;

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools, dead_code)]
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub valkey_url: String,
    pub minio_endpoint: String,
    pub minio_access_key: String,
    pub minio_secret_key: String,
    /// Accept self-signed TLS certificates for `MinIO` (dev/test only). S55.
    pub minio_insecure: bool,
    pub master_key: Option<String>,
    pub git_repos_path: PathBuf,
    pub ops_repos_path: PathBuf,
    pub smtp_host: Option<String>,
    pub smtp_port: u16,
    pub smtp_from: String,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub admin_password: Option<String>,
    pub pipeline_namespace: String,
    pub agent_namespace: String,
    pub registry_url: Option<String>,
    pub secure_cookies: bool,
    pub cors_origins: Vec<String>,
    pub trust_proxy_headers: bool,
    pub dev_mode: bool,
    /// Permission cache TTL in seconds (default 300 = 5 minutes).
    pub permission_cache_ttl_secs: u64,
    /// `WebAuthn` Relying Party ID (domain, no protocol).
    pub webauthn_rp_id: String,
    /// `WebAuthn` Relying Party Origin (full URL).
    pub webauthn_rp_origin: String,
    /// `WebAuthn` Relying Party display name.
    pub webauthn_rp_name: String,
    /// Platform API URL for agent/pipeline pods to reach the platform.
    pub platform_api_url: String,
    /// K8s namespace where the platform itself runs (for `NetworkPolicy` egress).
    pub platform_namespace: String,
    /// SSH server listen address (e.g. "0.0.0.0:2222"). `None` disables SSH.
    pub ssh_listen: Option<String>,
    /// Path to ED25519 host key (auto-generated if absent).
    pub ssh_host_key_path: String,
    /// Maximum concurrent CLI subprocess sessions per platform pod.
    pub max_cli_subprocesses: usize,
    /// Valkey host:port as seen from inside agent pods.
    /// Defaults to host:port parsed from `VALKEY_URL`.
    /// Override when platform connects via port-forward but agents use K8s DNS.
    /// Example: `"valkey.platform.svc.cluster.local:6379"`
    pub valkey_agent_host: String,
    /// Directory containing cross-compiled agent-runner binaries.
    /// Expected layout: `{dir}/amd64`, `{dir}/arm64`
    pub agent_runner_dir: PathBuf,
    /// Directory containing cross-compiled platform-proxy binaries.
    /// Expected layout: `{dir}/amd64`, `{dir}/arm64`
    pub proxy_binary_dir: PathBuf,
    /// Path to the MCP servers tarball served to agent pods at startup.
    /// Built by `just build` / `hack/build-agent-images.sh`.
    pub mcp_servers_tarball: PathBuf,
    /// Claude CLI version for auto-setup in agent pods.
    /// Used by the setup init container: `npm install -g @anthropic-ai/claude-code@<version>`.
    pub claude_cli_version: String,
    /// Optional namespace prefix for test isolation.
    /// When set, project namespaces become `{ns_prefix}-{slug}-{env}` instead of `{slug}-{env}`.
    pub ns_prefix: Option<String>,
    /// Whether to spawn real CLI subprocesses (default `true`).
    /// Set to `false` in integration tests to avoid spawning real `claude` processes.
    pub cli_spawn_enabled: bool,
    /// Registry URL as seen from K8s nodes (via `DaemonSet` proxy).
    /// Falls back to `registry_url` if unset.
    pub registry_node_url: Option<String>,
    /// Directory containing OCI layout tarballs to seed into the registry on startup.
    pub seed_images_path: PathBuf,
    /// Directory containing `.md` command templates to seed as global commands on startup.
    pub seed_commands_path: PathBuf,
    /// Health check interval in seconds (default 15).
    pub health_check_interval_secs: u64,
    /// Minimum tracing level for platform self-observability (default "warn").
    pub self_observe_level: String,
    /// Idle timeout for agent sessions in seconds (default 1800 = 30 min).
    /// Sessions with no messages for this duration are auto-completed by the reaper.
    pub session_idle_timeout_secs: u64,
    /// External URL for preview proxy (dev only).
    /// When set, preview requests route through this proxy instead of direct K8s DNS.
    /// Example: `http://172.18.0.2:31500`
    pub preview_proxy_url: Option<String>,
    /// Maximum concurrent pipeline step pods per pipeline (default 4).
    pub pipeline_max_parallel: usize,
    /// Name of the shared Gateway resource for traffic splitting (default "platform-gateway").
    pub gateway_name: String,
    /// Namespace where the shared Gateway lives (default: same as `platform_namespace`).
    pub gateway_namespace: String,
    /// Maximum pipeline run duration in seconds (default 3600 = 1 hour).
    pub pipeline_timeout_secs: u64,
    /// Maximum allowed LFS object size in bytes (default 5 GB).
    pub max_lfs_object_bytes: u64,
    /// Maximum API token expiry in days (default 365). S71.
    pub token_max_expiry_days: u32,
    /// Observability data retention in days (default 30). S94.
    pub observe_retention_days: u32,
    /// Previous master key for key rotation (S44). Optional — only during rotation.
    pub master_key_previous: Option<String>,
    /// Trusted proxy CIDRs (S59). When non-empty, X-Forwarded-For only trusted from these IPs.
    pub trust_proxy_cidrs: Vec<String>,
    /// Default runner image for agent pods (A4). Pinned to avoid `:latest`.
    pub runner_image: String,
    /// Git clone init container image (A4). Pinned to avoid `:latest`.
    pub git_clone_image: String,
    /// Kaniko image for imagebuild pipeline steps (A4). Pinned to avoid `:latest`.
    pub kaniko_image: String,
    /// When true, stream registry blobs through the platform instead of redirecting to `MinIO`.
    /// Needed when `MinIO` is not directly reachable from registry clients (e.g. kaniko in pods).
    pub registry_proxy_blobs: bool,
    /// Directory containing MCP server scripts for manager agent sessions.
    pub mcp_servers_path: String,
    /// Maximum single artifact file size in bytes (default 50 MB).
    pub max_artifact_file_bytes: u64,
    /// Maximum total artifact size per step in bytes (default 500 MB).
    pub max_artifact_total_bytes: u64,
    /// Maximum HTTP body size for registry blob uploads in bytes (default 2 GB).
    pub registry_http_body_limit_bytes: usize,
    /// Maximum individual registry blob size in bytes (default 5 GB).
    pub registry_max_blob_size_bytes: u64,
    /// Enable the mesh CA module (default: false).
    pub mesh_enabled: bool,
    /// Leaf certificate TTL in seconds (default: 3600 = 1 hour).
    pub mesh_ca_cert_ttl_secs: u64,
    /// Root CA certificate validity in days (default: 365).
    pub mesh_ca_root_ttl_days: u32,
    /// Path to prebuilt platform-proxy binary directory (dev/test only).
    pub proxy_binary_path: Option<String>,
    /// Whether the platform should auto-deploy its gateway DaemonSet/Deployment (default: false).
    pub gateway_auto_deploy: bool,
    /// Gateway HTTP listen port inside the pod (default: 8080).
    pub gateway_http_port: u16,
    /// Gateway TLS listen port inside the pod (default: 8443).
    pub gateway_tls_port: u16,
    /// `NodePort` for gateway HTTP (0 = K8s auto-assign). Default: 0.
    pub gateway_http_node_port: u16,
    /// `NodePort` for gateway TLS (0 = K8s auto-assign). Default: 0.
    pub gateway_tls_node_port: u16,
    /// Comma-separated namespaces the gateway should watch. Empty = all labeled.
    pub gateway_watch_namespaces: Vec<String>,
    /// Enable ACME (Let's Encrypt) automatic certificate provisioning (default: false).
    pub acme_enabled: bool,
    /// ACME directory URL (default: Let's Encrypt production).
    pub acme_directory_url: String,
    /// ACME contact email for account registration.
    pub acme_contact_email: Option<String>,
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

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("database_url", &"[REDACTED]")
            .field("valkey_url", &"[REDACTED]")
            .field("minio_endpoint", &self.minio_endpoint)
            .field("minio_access_key", &"[REDACTED]")
            .field("minio_secret_key", &"[REDACTED]")
            .field(
                "master_key",
                &self.master_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "smtp_password",
                &self.smtp_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "admin_password",
                &self.admin_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("dev_mode", &self.dev_mode)
            .field("secure_cookies", &self.secure_cookies)
            .field("pipeline_namespace", &self.pipeline_namespace)
            .field("agent_namespace", &self.agent_namespace)
            .field("platform_namespace", &self.platform_namespace)
            .field("runner_image", &self.runner_image)
            .field("git_clone_image", &self.git_clone_image)
            .field("kaniko_image", &self.kaniko_image)
            .finish_non_exhaustive()
    }
}

impl Config {
    #[allow(clippy::too_many_lines)]
    pub fn load() -> Self {
        let valkey_url = env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let valkey_agent_host = env::var("PLATFORM_VALKEY_AGENT_HOST")
            .unwrap_or_else(|_| derive_valkey_host_port(&valkey_url));
        Self {
            listen: env::var("PLATFORM_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://platform:dev@localhost:5432/platform_dev".into()),
            valkey_url,
            minio_endpoint: env::var("MINIO_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:9000".into()),
            minio_access_key: env::var("MINIO_ACCESS_KEY").unwrap_or_else(|_| "platform".into()),
            minio_secret_key: env::var("MINIO_SECRET_KEY").unwrap_or_else(|_| "devdevdev".into()),
            minio_insecure: env::var("MINIO_INSECURE").ok().is_some_and(|v| v == "true"),
            master_key: env::var("PLATFORM_MASTER_KEY").ok(),
            git_repos_path: env::var("PLATFORM_GIT_REPOS_PATH")
                .map_or_else(|_| PathBuf::from("/data/repos"), PathBuf::from),
            ops_repos_path: env::var("PLATFORM_OPS_REPOS_PATH")
                .map_or_else(|_| PathBuf::from("/data/ops-repos"), PathBuf::from),
            smtp_host: env::var("PLATFORM_SMTP_HOST").ok(),
            smtp_port: env::var("PLATFORM_SMTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(587),
            smtp_from: env::var("PLATFORM_SMTP_FROM")
                .unwrap_or_else(|_| "platform@localhost".into()),
            smtp_username: env::var("PLATFORM_SMTP_USERNAME").ok(),
            smtp_password: env::var("PLATFORM_SMTP_PASSWORD").ok(),
            admin_password: env::var("PLATFORM_ADMIN_PASSWORD").ok(),
            pipeline_namespace: env::var("PLATFORM_PIPELINE_NAMESPACE")
                .unwrap_or_else(|_| "platform-pipelines".into()),
            agent_namespace: env::var("PLATFORM_AGENT_NAMESPACE")
                .unwrap_or_else(|_| "platform-agents".into()),
            registry_url: env::var("PLATFORM_REGISTRY_URL").ok(),
            secure_cookies: env::var("PLATFORM_SECURE_COOKIES")
                .ok()
                .is_some_and(|v| v == "true"),
            cors_origins: env::var("PLATFORM_CORS_ORIGINS")
                .ok()
                .map_or_else(Vec::new, |v| parse_cors_origins(&v)),
            trust_proxy_headers: env::var("PLATFORM_TRUST_PROXY")
                .ok()
                .is_some_and(|v| v == "true"),
            dev_mode: env::var("PLATFORM_DEV").ok().is_some_and(|v| v == "true"),
            permission_cache_ttl_secs: env::var("PLATFORM_PERMISSION_CACHE_TTL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            webauthn_rp_id: env::var("WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".into()),
            webauthn_rp_origin: env::var("WEBAUTHN_RP_ORIGIN")
                .unwrap_or_else(|_| "http://localhost:8080".into()),
            webauthn_rp_name: env::var("WEBAUTHN_RP_NAME").unwrap_or_else(|_| "Platform".into()),
            platform_api_url: env::var("PLATFORM_API_URL")
                .unwrap_or_else(|_| "http://platform.platform.svc.cluster.local:8080".into()),
            platform_namespace: env::var("PLATFORM_NAMESPACE")
                .unwrap_or_else(|_| "platform".into()),
            ssh_listen: env::var("PLATFORM_SSH_LISTEN").ok(),
            ssh_host_key_path: env::var("PLATFORM_SSH_HOST_KEY_PATH")
                .unwrap_or_else(|_| "/data/ssh_host_ed25519_key".into()),
            max_cli_subprocesses: env::var("PLATFORM_MAX_CLI_SUBPROCESSES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            valkey_agent_host,
            agent_runner_dir: env::var("PLATFORM_AGENT_RUNNER_DIR")
                .map_or_else(|_| PathBuf::from("/data/agent-runner"), PathBuf::from),
            proxy_binary_dir: env::var("PLATFORM_PROXY_BINARY_DIR")
                .map_or_else(|_| PathBuf::from("/data/platform-proxy"), PathBuf::from),
            mcp_servers_tarball: env::var("PLATFORM_MCP_SERVERS_TARBALL")
                .map_or_else(|_| PathBuf::from("/data/mcp-servers.tar.gz"), PathBuf::from),
            claude_cli_version: env::var("PLATFORM_CLAUDE_CLI_VERSION")
                .unwrap_or_else(|_| "stable".into()),
            ns_prefix: env::var("PLATFORM_NS_PREFIX").ok(),
            cli_spawn_enabled: env::var("PLATFORM_CLI_SPAWN_ENABLED").ok().as_deref()
                != Some("false"),
            registry_node_url: env::var("PLATFORM_REGISTRY_NODE_URL").ok(),
            seed_images_path: env::var("PLATFORM_SEED_IMAGES_PATH")
                .map_or_else(|_| PathBuf::from("/data/seed-images"), PathBuf::from),
            seed_commands_path: env::var("PLATFORM_SEED_COMMANDS_PATH")
                .map_or_else(|_| PathBuf::from("/data/seed-commands"), PathBuf::from),
            health_check_interval_secs: env::var("PLATFORM_HEALTH_CHECK_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(15),
            self_observe_level: env::var("PLATFORM_SELF_OBSERVE_LEVEL")
                .unwrap_or_else(|_| "warn".into()),
            session_idle_timeout_secs: env::var("PLATFORM_SESSION_IDLE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1800),
            preview_proxy_url: env::var("PLATFORM_PREVIEW_PROXY_URL").ok(),
            pipeline_max_parallel: env::var("PLATFORM_PIPELINE_MAX_PARALLEL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            gateway_name: env::var("PLATFORM_GATEWAY_NAME")
                .unwrap_or_else(|_| "platform-gateway".into()),
            gateway_namespace: env::var("PLATFORM_GATEWAY_NAMESPACE").unwrap_or_else(|_| {
                env::var("PLATFORM_NAMESPACE").unwrap_or_else(|_| "platform".into())
            }),
            pipeline_timeout_secs: env::var("PLATFORM_PIPELINE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600),
            max_lfs_object_bytes: env::var("PLATFORM_MAX_LFS_OBJECT_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5_368_709_120),
            token_max_expiry_days: env::var("PLATFORM_TOKEN_MAX_EXPIRY_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(365),
            observe_retention_days: env::var("PLATFORM_OBSERVE_RETENTION_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            master_key_previous: env::var("PLATFORM_MASTER_KEY_PREVIOUS").ok(),
            trust_proxy_cidrs: env::var("PLATFORM_TRUST_PROXY_CIDR")
                .ok()
                .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect())
                .unwrap_or_default(),
            runner_image: env::var("PLATFORM_RUNNER_IMAGE")
                .unwrap_or_else(|_| "platform-runner:v1".into()),
            git_clone_image: env::var("PLATFORM_GIT_CLONE_IMAGE")
                .unwrap_or_else(|_| "alpine/git:2.47.2".into()),
            kaniko_image: env::var("PLATFORM_KANIKO_IMAGE")
                .unwrap_or_else(|_| "gcr.io/kaniko-project/executor:v1.23.2-debug".into()),
            registry_proxy_blobs: env::var("REGISTRY_PROXY_BLOBS")
                .ok()
                .is_some_and(|v| v == "true"),
            mcp_servers_path: {
                let p =
                    env::var("PLATFORM_MCP_SERVERS_PATH").unwrap_or_else(|_| "mcp/servers".into());
                // Resolve to absolute path so MCP servers work when CLI CWD is /tmp
                let path = PathBuf::from(&p);
                if path.is_absolute() {
                    p
                } else {
                    env::current_dir()
                        .map(|cwd| cwd.join(&path).to_string_lossy().into_owned())
                        .unwrap_or(p)
                }
            },
            max_artifact_file_bytes: env::var("PLATFORM_MAX_ARTIFACT_FILE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50 * 1024 * 1024), // 50 MB
            max_artifact_total_bytes: env::var("PLATFORM_MAX_ARTIFACT_TOTAL_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500 * 1024 * 1024), // 500 MB
            registry_http_body_limit_bytes: env::var("PLATFORM_REGISTRY_HTTP_BODY_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2 * 1024 * 1024 * 1024), // 2 GB
            registry_max_blob_size_bytes: env::var("PLATFORM_REGISTRY_MAX_BLOB_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5_368_709_120), // 5 GB
            mesh_enabled: env::var("PLATFORM_MESH_ENABLED")
                .ok()
                .is_some_and(|v| v == "true"),
            mesh_ca_cert_ttl_secs: env::var("PLATFORM_MESH_CERT_TTL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600),
            mesh_ca_root_ttl_days: env::var("PLATFORM_MESH_CA_ROOT_TTL_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(365),
            proxy_binary_path: env::var("PLATFORM_PROXY_PATH").ok(),
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
        }
    }

    /// Derive a project's K8s namespace: `{ns_prefix}-{slug}-{env}` or `{slug}-{env}`.
    pub fn project_namespace(&self, slug: &str, env: &str) -> String {
        match &self.ns_prefix {
            Some(prefix) => format!("{prefix}-{slug}-{env}"),
            None => format!("{slug}-{env}"),
        }
    }
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

#[cfg(test)]
impl Config {
    /// Sensible defaults for unit tests. Override specific fields with struct update syntax:
    /// `Config { smtp_host: Some("localhost".into()), ..Config::test_default() }`
    pub fn test_default() -> Self {
        Self {
            listen: "127.0.0.1:0".into(),
            database_url: "postgres://localhost/test".into(),
            valkey_url: "redis://localhost:6379".into(),
            minio_endpoint: "http://localhost:9000".into(),
            minio_access_key: "test".into(),
            minio_secret_key: "test".into(),
            minio_insecure: false,
            master_key: None,
            git_repos_path: "/tmp/repos".into(),
            ops_repos_path: "/tmp/ops-repos".into(),
            smtp_host: None,
            smtp_port: 587,
            smtp_from: "test@localhost".into(),
            smtp_username: None,
            smtp_password: None,
            admin_password: None,
            pipeline_namespace: "test-pipelines".into(),
            agent_namespace: "test-agents".into(),
            registry_url: None,
            secure_cookies: false,
            cors_origins: vec![],
            trust_proxy_headers: false,
            dev_mode: true,
            permission_cache_ttl_secs: 300,
            webauthn_rp_id: "localhost".into(),
            webauthn_rp_origin: "http://localhost:8080".into(),
            webauthn_rp_name: "Test Platform".into(),
            platform_api_url: "http://platform.test-agents.svc.cluster.local:8080".into(),
            platform_namespace: "test-platform".into(),
            ssh_listen: None,
            ssh_host_key_path: "/tmp/test_ssh_host_key".into(),
            max_cli_subprocesses: 10,
            valkey_agent_host: "localhost:6379".into(),
            agent_runner_dir: "/tmp/test-agent-runner".into(),
            proxy_binary_dir: "/tmp/test-platform-proxy".into(),
            mcp_servers_tarball: "/tmp/test-mcp-servers.tar.gz".into(),
            claude_cli_version: "stable".into(),
            ns_prefix: None,
            cli_spawn_enabled: true,
            registry_node_url: None,
            seed_images_path: "/tmp/seed-images".into(),
            seed_commands_path: "/tmp/seed-commands".into(),
            health_check_interval_secs: 15,
            self_observe_level: "warn".into(),
            session_idle_timeout_secs: 1800,
            preview_proxy_url: None,
            pipeline_max_parallel: 4,
            gateway_name: "platform-gateway".into(),
            gateway_namespace: "test-platform".into(),
            pipeline_timeout_secs: 3600,
            max_lfs_object_bytes: 5_368_709_120,
            token_max_expiry_days: 365,
            observe_retention_days: 30,
            master_key_previous: None,
            trust_proxy_cidrs: vec![],
            runner_image: "platform-runner:v1".into(),
            git_clone_image: "alpine/git:2.47.2".into(),
            kaniko_image: "gcr.io/kaniko-project/executor:v1.23.2-debug".into(),
            registry_proxy_blobs: false,
            mcp_servers_path: "mcp/servers".into(),
            max_artifact_file_bytes: 50 * 1024 * 1024,
            max_artifact_total_bytes: 500 * 1024 * 1024,
            registry_http_body_limit_bytes: 2 * 1024 * 1024 * 1024, // 2 GB
            registry_max_blob_size_bytes: 5_368_709_120,            // 5 GB
            mesh_enabled: false,
            mesh_ca_cert_ttl_secs: 3600,
            mesh_ca_root_ttl_days: 365,
            proxy_binary_path: None,
            gateway_auto_deploy: false,
            gateway_http_port: 8080,
            gateway_tls_port: 8443,
            gateway_http_node_port: 0,
            gateway_tls_node_port: 0,
            gateway_watch_namespaces: vec![],
            acme_enabled: false,
            acme_directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
            acme_contact_email: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cors_origins_single() {
        let result = parse_cors_origins("http://localhost:3000");
        assert_eq!(result, vec!["http://localhost:3000"]);
    }

    #[test]
    fn parse_cors_origins_multiple_with_spaces() {
        let result = parse_cors_origins("http://a.com, http://b.com , http://c.com");
        assert_eq!(result, vec!["http://a.com", "http://b.com", "http://c.com"]);
    }

    #[test]
    fn parse_cors_origins_empty_string() {
        let result = parse_cors_origins("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_default_smtp_port() {
        let config = Config::test_default();
        assert_eq!(config.smtp_port, 587);
    }

    #[test]
    fn test_default_pipeline_namespace() {
        let config = Config::test_default();
        assert_eq!(config.pipeline_namespace, "test-pipelines");
    }

    #[test]
    fn test_default_has_dev_mode() {
        let config = Config::test_default();
        assert!(config.dev_mode);
        assert!(!config.secure_cookies);
        assert!(!config.trust_proxy_headers);
    }

    #[test]
    fn parse_cors_origins_whitespace_only_is_empty() {
        let result = parse_cors_origins("  ");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_cors_origins_whitespace_trimmed() {
        let result = parse_cors_origins(" a.com , b.com ");
        assert_eq!(result, vec!["a.com", "b.com"]);
    }

    #[test]
    fn test_default_cors_origins_empty() {
        let config = Config::test_default();
        assert!(
            config.cors_origins.is_empty(),
            "test_default should have no CORS origins"
        );
    }

    #[test]
    fn test_default_agent_namespace() {
        let config = Config::test_default();
        assert_eq!(config.agent_namespace, "test-agents");
    }

    #[test]
    fn test_default_webauthn_defaults() {
        let config = Config::test_default();
        assert_eq!(config.webauthn_rp_id, "localhost");
        assert_eq!(config.webauthn_rp_origin, "http://localhost:8080");
        assert_eq!(config.webauthn_rp_name, "Test Platform");
    }

    #[test]
    fn test_default_no_master_key() {
        let config = Config::test_default();
        assert!(
            config.master_key.is_none(),
            "test_default should have no master key"
        );
    }

    #[test]
    fn test_default_ssh_listen_none() {
        let config = Config::test_default();
        assert!(
            config.ssh_listen.is_none(),
            "test_default should have SSH disabled"
        );
    }

    #[test]
    fn test_default_ssh_host_key_path() {
        let config = Config::test_default();
        assert_eq!(config.ssh_host_key_path, "/tmp/test_ssh_host_key");
    }

    #[test]
    fn parse_cors_origins_trailing_comma() {
        let result = parse_cors_origins("a.com,b.com,");
        assert_eq!(result, vec!["a.com", "b.com"]);
    }

    #[test]
    fn test_default_platform_api_url() {
        let config = Config::test_default();
        assert!(config.platform_api_url.starts_with("http://"));
    }

    #[test]
    fn platform_api_url_default_value() {
        let config = Config::load();
        // Unless PLATFORM_API_URL is set, defaults to cluster-internal URL
        assert!(!config.platform_api_url.is_empty());
    }

    #[test]
    fn config_load_does_not_panic() {
        let config = Config::load();
        assert!(!config.listen.is_empty());
        assert!(!config.database_url.is_empty());
        assert!(!config.valkey_url.is_empty());
    }

    #[test]
    fn config_load_defaults_match_expected() {
        let config = Config::load();
        // smtp_port defaults to 587 unless PLATFORM_SMTP_PORT is set
        assert!(config.smtp_port > 0);
        // Permission cache TTL defaults to 300 unless PLATFORM_PERMISSION_CACHE_TTL is set
        assert!(config.permission_cache_ttl_secs > 0);
        // webauthn fields are always populated
        assert!(!config.webauthn_rp_id.is_empty());
        assert!(!config.webauthn_rp_origin.is_empty());
        assert!(!config.webauthn_rp_name.is_empty());
    }

    #[test]
    fn test_default_valkey_agent_host() {
        let config = Config::test_default();
        assert_eq!(config.valkey_agent_host, "localhost:6379");
    }

    #[test]
    fn derive_valkey_host_port_from_url() {
        assert_eq!(
            derive_valkey_host_port("redis://myhost:7000"),
            "myhost:7000"
        );
    }

    #[test]
    fn derive_valkey_host_port_default_port() {
        assert_eq!(derive_valkey_host_port("redis://myhost"), "myhost:6379");
    }

    #[test]
    fn derive_valkey_host_port_with_auth() {
        assert_eq!(
            derive_valkey_host_port("redis://user:pass@myhost:6380"),
            "myhost:6380"
        );
    }

    #[test]
    fn derive_valkey_host_port_invalid_url() {
        assert_eq!(derive_valkey_host_port("not-a-url"), "localhost:6379");
    }

    #[test]
    fn test_default_agent_runner_dir() {
        let config = Config::test_default();
        assert_eq!(
            config.agent_runner_dir,
            std::path::PathBuf::from("/tmp/test-agent-runner")
        );
    }

    #[test]
    fn test_default_claude_cli_version() {
        let config = Config::test_default();
        assert_eq!(config.claude_cli_version, "stable");
    }

    #[test]
    fn config_load_agent_runner_defaults() {
        let config = Config::load();
        assert!(!config.claude_cli_version.is_empty());
        assert!(!config.agent_runner_dir.as_os_str().is_empty());
    }

    #[test]
    fn project_namespace_without_prefix() {
        let config = Config::test_default();
        assert_eq!(config.project_namespace("my-app", "dev"), "my-app-dev");
        assert_eq!(config.project_namespace("my-app", "prod"), "my-app-prod");
    }

    #[test]
    fn project_namespace_with_prefix() {
        let config = Config {
            ns_prefix: Some("platform-test-abc123".into()),
            ..Config::test_default()
        };
        assert_eq!(
            config.project_namespace("my-app", "dev"),
            "platform-test-abc123-my-app-dev"
        );
    }

    #[test]
    fn test_default_ns_prefix_is_none() {
        let config = Config::test_default();
        assert!(config.ns_prefix.is_none());
    }

    #[test]
    fn test_default_session_idle_timeout() {
        let config = Config::test_default();
        assert_eq!(config.session_idle_timeout_secs, 1800);
    }

    #[test]
    fn config_load_optional_fields() {
        let config = Config::load();
        // These are populated from env vars or None — just verify they don't panic
        let _ = config.master_key;
        let _ = config.smtp_host;
        let _ = config.registry_url;
        let _ = config.admin_password;
        // cors_origins should be a Vec (empty or populated)
        let _ = config.cors_origins.len();
    }

    #[test]
    fn config_debug_redacts_sensitive_fields() {
        let config = Config {
            database_url: "postgres://secret:password@db:5432/prod".into(),
            valkey_url: "redis://:hunter2@valkey:6379".into(),
            minio_access_key: "super-secret-minio-access".into(),
            minio_secret_key: "super-secret-minio-key".into(),
            master_key: Some("0123456789abcdef".into()),
            smtp_password: Some("smtp-secret".into()),
            admin_password: Some("admin-secret".into()),
            ..Config::test_default()
        };
        let debug = format!("{config:?}");
        // Sensitive values must NOT appear
        assert!(!debug.contains("secret:password"), "database_url leaked");
        assert!(!debug.contains("hunter2"), "valkey_url leaked");
        assert!(
            !debug.contains("super-secret-minio-access"),
            "minio_access_key leaked"
        );
        assert!(
            !debug.contains("super-secret-minio-key"),
            "minio_secret_key leaked"
        );
        assert!(!debug.contains("0123456789abcdef"), "master_key leaked");
        assert!(!debug.contains("smtp-secret"), "smtp_password leaked");
        assert!(!debug.contains("admin-secret"), "admin_password leaked");
        // Redaction markers must appear
        assert!(debug.contains("[REDACTED]"), "missing [REDACTED] markers");
    }

    #[test]
    fn config_debug_shows_non_sensitive_fields() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(debug.contains("127.0.0.1:0"), "listen should be visible");
        assert!(
            debug.contains("dev_mode"),
            "dev_mode field should be visible"
        );
    }

    #[test]
    fn config_debug_shows_platform_namespace() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("platform_namespace"),
            "platform_namespace field should be visible"
        );
        assert!(
            debug.contains("test-platform"),
            "platform_namespace value should be visible"
        );
    }

    #[test]
    fn config_debug_shows_runner_image() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("runner_image"),
            "runner_image field should be visible"
        );
        assert!(
            debug.contains("platform-runner:v1"),
            "runner_image value should be visible"
        );
    }

    #[test]
    fn config_debug_shows_kaniko_image() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("kaniko_image"),
            "kaniko_image field should be visible"
        );
    }

    #[test]
    fn config_debug_shows_git_clone_image() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("git_clone_image"),
            "git_clone_image field should be visible"
        );
    }

    #[test]
    fn config_debug_redacts_master_key_none() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        // master_key is None, should show "None" not "[REDACTED]"
        assert!(
            debug.contains("master_key: None"),
            "master_key None should be shown, got: {debug}"
        );
    }

    #[test]
    fn config_debug_redacts_smtp_password_none() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("smtp_password: None"),
            "smtp_password None should be shown"
        );
    }

    #[test]
    fn config_debug_redacts_admin_password_none() {
        let config = Config::test_default();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("admin_password: None"),
            "admin_password None should be shown"
        );
    }

    #[test]
    fn parse_cors_origins_commas_only() {
        let result = parse_cors_origins(",,,");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_cors_origins_mixed_empty_and_valid() {
        let result = parse_cors_origins(",a.com,,b.com,");
        assert_eq!(result, vec!["a.com", "b.com"]);
    }

    #[test]
    fn parse_cors_origins_tabs_and_spaces() {
        let result = parse_cors_origins("  a.com ,\tb.com\t");
        assert_eq!(result, vec!["a.com", "b.com"]);
    }

    #[test]
    fn derive_valkey_host_port_with_path() {
        // Redis URLs can have /0, /1 etc. for database selection
        assert_eq!(
            derive_valkey_host_port("redis://myhost:6379/0"),
            "myhost:6379"
        );
    }

    #[test]
    fn config_test_default_all_fields_populated() {
        let config = Config::test_default();
        assert!(!config.listen.is_empty());
        assert!(!config.database_url.is_empty());
        assert!(!config.valkey_url.is_empty());
        assert!(!config.minio_endpoint.is_empty());
        assert!(!config.minio_access_key.is_empty());
        assert!(!config.minio_secret_key.is_empty());
        assert!(!config.smtp_from.is_empty());
        assert!(!config.pipeline_namespace.is_empty());
        assert!(!config.agent_namespace.is_empty());
        assert!(!config.webauthn_rp_id.is_empty());
        assert!(!config.webauthn_rp_origin.is_empty());
        assert!(!config.webauthn_rp_name.is_empty());
        assert!(!config.platform_api_url.is_empty());
        assert!(!config.platform_namespace.is_empty());
        assert!(!config.ssh_host_key_path.is_empty());
        assert!(config.max_cli_subprocesses > 0);
        assert!(!config.valkey_agent_host.is_empty());
        assert!(!config.claude_cli_version.is_empty());
        assert!(!config.self_observe_level.is_empty());
        assert!(config.session_idle_timeout_secs > 0);
        assert!(config.pipeline_max_parallel > 0);
        assert!(!config.gateway_name.is_empty());
        assert!(!config.gateway_namespace.is_empty());
        assert!(config.pipeline_timeout_secs > 0);
        assert!(config.max_lfs_object_bytes > 0);
        assert!(config.token_max_expiry_days > 0);
        assert!(config.observe_retention_days > 0);
        assert!(!config.runner_image.is_empty());
        assert!(!config.git_clone_image.is_empty());
        assert!(!config.kaniko_image.is_empty());
    }

    #[test]
    fn config_test_default_optional_fields_none() {
        let config = Config::test_default();
        assert!(config.master_key.is_none());
        assert!(config.smtp_host.is_none());
        assert!(config.smtp_username.is_none());
        assert!(config.smtp_password.is_none());
        assert!(config.admin_password.is_none());
        assert!(config.registry_url.is_none());
        assert!(config.ssh_listen.is_none());
        assert!(config.ns_prefix.is_none());
        assert!(config.registry_node_url.is_none());
        assert!(config.preview_proxy_url.is_none());
        assert!(config.master_key_previous.is_none());
    }

    #[test]
    fn config_test_default_bool_fields() {
        let config = Config::test_default();
        assert!(!config.minio_insecure);
        assert!(!config.secure_cookies);
        assert!(!config.trust_proxy_headers);
        assert!(config.dev_mode);
        assert!(config.cli_spawn_enabled);
    }
}
