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
    pub smtp_host: Option<String>,
    pub smtp_port: u16,
    pub smtp_from: String,
    pub admin_password: Option<String>,
    pub pipeline_namespace: String,
    pub agent_namespace: String,
    pub registry_url: Option<String>,
    pub secure_cookies: bool,
    pub cors_origins: Vec<String>,
    pub trust_proxy_headers: bool,
    pub dev_mode: bool,
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
            smtp_host: env::var("PLATFORM_SMTP_HOST").ok(),
            smtp_port: env::var("PLATFORM_SMTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(587),
            smtp_from: env::var("PLATFORM_SMTP_FROM")
                .unwrap_or_else(|_| "platform@localhost".into()),
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
                .map_or_else(Vec::new, |v| {
                    v.split(',').map(|s| s.trim().to_owned()).collect()
                }),
            trust_proxy_headers: env::var("PLATFORM_TRUST_PROXY")
                .ok()
                .is_some_and(|v| v == "true"),
            dev_mode: env::var("PLATFORM_DEV").ok().is_some_and(|v| v == "true"),
        }
    }
}
