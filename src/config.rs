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
        }
    }
}
