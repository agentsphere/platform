use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by modules 03-09
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
    /// `WebAuthn` Relying Party ID (domain, no protocol).
    pub webauthn_rp_id: String,
    /// `WebAuthn` Relying Party Origin (full URL).
    pub webauthn_rp_origin: String,
    /// `WebAuthn` Relying Party display name.
    pub webauthn_rp_name: String,
}

fn parse_cors_origins(s: &str) -> Vec<String> {
    s.split(',').map(|s| s.trim().to_owned()).collect()
}

impl Config {
    pub fn load() -> Self {
        Self {
            listen: env::var("PLATFORM_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://platform:dev@localhost:5432/platform_dev".into()),
            valkey_url: env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into()),
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
            webauthn_rp_id: env::var("WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".into()),
            webauthn_rp_origin: env::var("WEBAUTHN_RP_ORIGIN")
                .unwrap_or_else(|_| "http://localhost:8080".into()),
            webauthn_rp_name: env::var("WEBAUTHN_RP_NAME").unwrap_or_else(|_| "Platform".into()),
        }
    }
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
            webauthn_rp_id: "localhost".into(),
            webauthn_rp_origin: "http://localhost:8080".into(),
            webauthn_rp_name: "Test Platform".into(),
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
        assert_eq!(result, vec![""]);
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
    fn parse_cors_origins_empty_produces_single_empty_string() {
        // Documenting current behavior: empty string produces vec![""]
        // This may be a bug — callers should handle empty CORS gracefully
        let result = parse_cors_origins("");
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn parse_cors_origins_whitespace_trimmed() {
        let result = parse_cors_origins(" a.com , b.com ");
        assert_eq!(result, vec!["a.com", "b.com"]);
    }

    #[test]
    fn test_default_cors_origins_empty() {
        let config = Config::test_default();
        assert!(config.cors_origins.is_empty(), "test_default should have no CORS origins");
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
        assert!(config.master_key.is_none(), "test_default should have no master key");
    }

    #[test]
    fn parse_cors_origins_trailing_comma() {
        let result = parse_cors_origins("a.com,b.com,");
        // Trailing comma produces an empty string at end
        assert_eq!(result, vec!["a.com", "b.com", ""]);
    }
}
