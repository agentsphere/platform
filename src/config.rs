use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
#[allow(dead_code, clippy::struct_excessive_bools)] // fields consumed by modules 03-09
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub valkey_url: String,
    pub minio_endpoint: String,
    pub minio_access_key: String,
    pub minio_secret_key: String,
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
    /// Health check interval in seconds (default 15).
    pub health_check_interval_secs: u64,
    /// Minimum tracing level for platform self-observability (default "warn").
    pub self_observe_level: String,
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

impl Config {
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
            claude_cli_version: env::var("PLATFORM_CLAUDE_CLI_VERSION")
                .unwrap_or_else(|_| "stable".into()),
            ns_prefix: env::var("PLATFORM_NS_PREFIX").ok(),
            cli_spawn_enabled: env::var("PLATFORM_CLI_SPAWN_ENABLED").ok().as_deref()
                != Some("false"),
            registry_node_url: env::var("PLATFORM_REGISTRY_NODE_URL").ok(),
            seed_images_path: env::var("PLATFORM_SEED_IMAGES_PATH")
                .map_or_else(|_| PathBuf::from("/data/seed-images"), PathBuf::from),
            health_check_interval_secs: env::var("PLATFORM_HEALTH_CHECK_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(15),
            self_observe_level: env::var("PLATFORM_SELF_OBSERVE_LEVEL")
                .unwrap_or_else(|_| "warn".into()),
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
            claude_cli_version: "stable".into(),
            ns_prefix: None,
            cli_spawn_enabled: true,
            registry_node_url: None,
            seed_images_path: "/tmp/seed-images".into(),
            health_check_interval_secs: 15,
            self_observe_level: "warn".into(),
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
}
