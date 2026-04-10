//! Proxy configuration from environment variables and CLI args.

use std::env;
use std::net::IpAddr;
use std::time::Duration;

/// Default TCP passthrough ports — known non-HTTP protocols that should skip mTLS.
const DEFAULT_PASSTHROUGH_PORTS: &str = "5432,6379,3306,27017,9042,5672,4222";

/// mTLS enforcement mode for transparent proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtlsMode {
    /// Accept both mTLS and plaintext connections.
    Permissive,
    /// Reject plaintext except from kubelet/node IPs.
    Strict,
}

impl MtlsMode {
    /// Parse from string; anything other than `"strict"` is permissive.
    pub fn from_str_value(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "strict" => Self::Strict,
            _ => Self::Permissive,
        }
    }
}

/// Proxy configuration parsed from env vars and CLI args.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Platform API URL for OTLP export and cert issuance.
    pub api_url: String,
    /// Bearer token for platform API auth.
    pub api_token: String,
    /// Dedicated OTLP bearer token (observe:write scope). Falls back to `api_token`.
    pub otlp_token: String,
    /// Project UUID for resource attribution.
    pub project_id: Option<String>,
    /// Service name (default: wrapped binary name).
    pub service_name: String,
    /// Optional session UUID.
    pub session_id: Option<String>,
    /// Health endpoint port (default 15020).
    pub health_port: u16,
    /// App HTTP port for readiness check and inbound forwarding.
    pub app_port: Option<u16>,
    /// Proxy's own log level (default info).
    pub log_level: String,
    /// Process metrics scrape interval.
    pub metrics_interval: Duration,
    /// OTLP batch flush interval.
    pub flush_interval: Duration,
    /// Max spans/logs per OTLP batch.
    pub batch_size: usize,

    // --- mTLS (PR 3) ---
    /// mTLS inbound listener port (e.g. 8443). None = disabled.
    pub tls_port: Option<u16>,
    /// mTLS outbound proxy port (e.g. 15001). None = disabled.
    pub outbound_port: Option<u16>,
    /// TCP proxy ports for non-HTTP (e.g. postgres 5432, redis 6379).
    pub tcp_ports: Vec<u16>,
    /// K8s namespace for SPIFFE identity.
    pub namespace: String,

    // --- Metric scraping (PR 3) ---
    /// Scraper type: "postgres", "redis", or None.
    pub scrape_type: Option<String>,
    /// Prometheus scrape URL (e.g. `MinIO` metrics endpoint).
    pub scrape_url: Option<String>,
    /// Postgres connection URL for stat queries.
    pub scrape_postgres_url: Option<String>,
    /// Redis/Valkey connection URL for INFO scraping.
    pub scrape_redis_url: Option<String>,
    /// Allow insecure TLS for scrape targets.
    pub scrape_tls_insecure: bool,

    // --- Transparent proxy mode ---
    /// Enable transparent proxy mode (iptables REDIRECT + `SO_ORIGINAL_DST`).
    pub transparent: bool,
    /// mTLS enforcement: permissive (default) or strict.
    pub mtls_mode: MtlsMode,
    /// Transparent inbound listener port (default 15006).
    pub inbound_port: u16,
    /// Source port range for proxy outbound connections to bypass iptables (default 61000-65000).
    pub bypass_port_range: (u16, u16),
    /// Internal CIDRs -- traffic to these destinations gets mTLS (default RFC1918).
    pub internal_cidrs: Vec<(IpAddr, u8)>,
    /// Node CIDRs -- kubelet IPs allowed plaintext even in strict mode.
    pub node_cidrs: Vec<(IpAddr, u8)>,
}

/// Parsed CLI arguments before env var merging.
struct CliArgs {
    app_port: Option<u16>,
    tcp_ports: Vec<u16>,
    scrape_type: Option<String>,
    scrape_url: Option<String>,
    scrape_tls_insecure: bool,
    child_args: Vec<String>,
}

/// Parse CLI args into structured form.
fn parse_cli_args(args: &[String]) -> CliArgs {
    let mut result = CliArgs {
        app_port: None,
        tcp_ports: Vec::new(),
        scrape_type: None,
        scrape_url: None,
        scrape_tls_insecure: false,
        child_args: Vec::new(),
    };
    let mut after_separator = false;
    let mut i = 0;
    while i < args.len() {
        if after_separator {
            result.child_args.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => after_separator = true,
            "--wrap" => {}
            s if s.starts_with("--app-port=") => {
                result.app_port = s.strip_prefix("--app-port=").and_then(|v| v.parse().ok());
            }
            "--app-port" if i + 1 < args.len() => {
                i += 1;
                result.app_port = args[i].parse().ok();
            }
            s if s.starts_with("--tcp-ports=") => {
                if let Some(val) = s.strip_prefix("--tcp-ports=") {
                    result.tcp_ports = val
                        .split(',')
                        .filter_map(|p| p.trim().parse().ok())
                        .collect();
                }
            }
            "--tcp-ports" if i + 1 < args.len() => {
                i += 1;
                result.tcp_ports = args[i]
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
            }
            s if s.starts_with("--scrape-type=") => {
                result.scrape_type = s.strip_prefix("--scrape-type=").map(ToString::to_string);
            }
            "--scrape-type" if i + 1 < args.len() => {
                i += 1;
                result.scrape_type = Some(args[i].clone());
            }
            s if s.starts_with("--scrape-url=") => {
                result.scrape_url = s.strip_prefix("--scrape-url=").map(ToString::to_string);
            }
            "--scrape-url" if i + 1 < args.len() => {
                i += 1;
                result.scrape_url = Some(args[i].clone());
            }
            "--scrape-tls-insecure" => result.scrape_tls_insecure = true,
            _ => {
                result.child_args.push(args[i].clone());
                after_separator = true;
            }
        }
        i += 1;
    }
    result
}

/// Derive the service name from env or child binary.
fn resolve_service_name(child_args: &[String]) -> String {
    env::var("PLATFORM_SERVICE_NAME").unwrap_or_else(|_| {
        child_args
            .first()
            .and_then(|s| {
                std::path::Path::new(s)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| "unknown".into())
    })
}

/// Read an env var, parse it, or return a default.
/// Parse a port range string like `"61000:65000"` into `(u16, u16)`.
fn parse_port_range(s: &str) -> Option<(u16, u16)> {
    let (lo, hi) = s.split_once(':')?;
    Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?))
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl ProxyConfig {
    /// Parse configuration from environment variables and CLI args.
    #[tracing::instrument(skip_all)]
    pub fn from_env_and_args(args: &[String]) -> (Self, Vec<String>) {
        let mut cli = parse_cli_args(args);
        let service_name = resolve_service_name(&cli.child_args);

        // Override from env if not set via CLI
        if cli.app_port.is_none() {
            cli.app_port = env::var("PROXY_APP_PORT").ok().and_then(|v| v.parse().ok());
        }
        if cli.scrape_type.is_none() {
            cli.scrape_type = env::var("PROXY_SCRAPE_TYPE").ok();
        }
        if cli.scrape_url.is_none() {
            cli.scrape_url = env::var("PROXY_SCRAPE_URL").ok();
        }

        let config = Self {
            api_url: env::var("PLATFORM_API_URL")
                .unwrap_or_else(|_| "http://platform.platform.svc.cluster.local:8080".into()),
            api_token: env::var("PLATFORM_API_TOKEN").unwrap_or_default(),
            otlp_token: env::var("OTEL_API_TOKEN")
                .or_else(|_| env::var("PLATFORM_API_TOKEN"))
                .unwrap_or_default(),
            project_id: env::var("PLATFORM_PROJECT_ID").ok(),
            service_name,
            session_id: env::var("PLATFORM_SESSION_ID").ok(),
            health_port: env_parse("PROXY_HEALTH_PORT", 15020),
            app_port: cli.app_port,
            log_level: env::var("PROXY_LOG_LEVEL").unwrap_or_else(|_| "info".into()),
            metrics_interval: Duration::from_secs(env_parse("PROXY_METRICS_INTERVAL", 15)),
            flush_interval: Duration::from_secs(env_parse("PROXY_FLUSH_INTERVAL", 5)),
            batch_size: env_parse("PROXY_BATCH_SIZE", 500),
            tls_port: env::var("PROXY_TLS_PORT").ok().and_then(|v| v.parse().ok()),
            outbound_port: env::var("PROXY_OUTBOUND_PORT")
                .ok()
                .and_then(|v| v.parse().ok()),
            tcp_ports: if cli.tcp_ports.is_empty() {
                super::transparent::parse_ports(
                    &env::var("PROXY_TCP_PORTS")
                        .unwrap_or_else(|_| DEFAULT_PASSTHROUGH_PORTS.into()),
                )
            } else {
                cli.tcp_ports
            },
            namespace: env::var("PROXY_NAMESPACE").unwrap_or_else(|_| "default".into()),
            scrape_type: cli.scrape_type,
            scrape_url: cli.scrape_url,
            scrape_postgres_url: env::var("PROXY_SCRAPE_POSTGRES_URL").ok(),
            scrape_redis_url: env::var("PROXY_SCRAPE_REDIS_URL").ok(),
            scrape_tls_insecure: cli.scrape_tls_insecure,
            transparent: env::var("PROXY_TRANSPARENT").is_ok_and(|v| v == "true" || v == "1"),
            mtls_mode: MtlsMode::from_str_value(&env::var("PROXY_MTLS_MODE").unwrap_or_default()),
            inbound_port: env_parse("PROXY_INBOUND_PORT", 15006),
            bypass_port_range: parse_port_range(
                &env::var("PROXY_BYPASS_PORT_RANGE").unwrap_or_default(),
            )
            .unwrap_or((
                super::transparent::BYPASS_PORT_MIN,
                super::transparent::BYPASS_PORT_MAX,
            )),
            internal_cidrs: super::transparent::parse_cidrs(
                &env::var("PROXY_INTERNAL_CIDRS")
                    .unwrap_or_else(|_| "10.0.0.0/8,172.16.0.0/12,192.168.0.0/16".into()),
            ),
            node_cidrs: super::transparent::parse_cidrs(
                &env::var("PROXY_NODE_CIDRS").unwrap_or_default(),
            ),
        };

        (config, cli.child_args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_args() {
        let args: Vec<String> = vec![
            "--wrap".into(),
            "--app-port=8080".into(),
            "--".into(),
            "postgres".into(),
            "-c".into(),
            "max_connections=300".into(),
        ];
        let (config, child) = ProxyConfig::from_env_and_args(&args);
        assert_eq!(config.app_port, Some(8080));
        assert_eq!(child, vec!["postgres", "-c", "max_connections=300"]);
    }

    #[test]
    fn parse_tcp_ports() {
        let args: Vec<String> = vec![
            "--wrap".into(),
            "--tcp-ports=5432,6379".into(),
            "--".into(),
            "service".into(),
        ];
        let (config, _) = ProxyConfig::from_env_and_args(&args);
        assert_eq!(config.tcp_ports, vec![5432, 6379]);
    }

    #[test]
    fn parse_scrape_args() {
        let args: Vec<String> = vec![
            "--wrap".into(),
            "--scrape-type=postgres".into(),
            "--scrape-url=http://localhost:9000/metrics".into(),
            "--scrape-tls-insecure".into(),
            "--".into(),
            "app".into(),
        ];
        let (config, _) = ProxyConfig::from_env_and_args(&args);
        assert_eq!(config.scrape_type, Some("postgres".into()));
        assert_eq!(
            config.scrape_url,
            Some("http://localhost:9000/metrics".into())
        );
        assert!(config.scrape_tls_insecure);
    }

    #[test]
    fn parse_separated_args() {
        let args: Vec<String> = vec![
            "--wrap".into(),
            "--app-port".into(),
            "3000".into(),
            "--tcp-ports".into(),
            "5432".into(),
            "--scrape-type".into(),
            "redis".into(),
            "--scrape-url".into(),
            "http://localhost:9000".into(),
            "--".into(),
            "myapp".into(),
        ];
        let (config, child) = ProxyConfig::from_env_and_args(&args);
        assert_eq!(config.app_port, Some(3000));
        assert_eq!(config.tcp_ports, vec![5432]);
        assert_eq!(config.scrape_type, Some("redis".into()));
        assert_eq!(config.scrape_url, Some("http://localhost:9000".into()));
        assert_eq!(child, vec!["myapp"]);
    }

    #[test]
    fn defaults_when_no_args() {
        let args: Vec<String> = vec!["--wrap".into(), "--".into(), "app".into()];
        let (config, child) = ProxyConfig::from_env_and_args(&args);
        assert_eq!(config.health_port, 15020);
        assert_eq!(config.batch_size, 500);
        assert!(config.tls_port.is_none());
        assert!(config.outbound_port.is_none());
        // Default passthrough ports (Postgres, Redis, MySQL, etc.)
        assert!(config.tcp_ports.contains(&5432));
        assert!(config.tcp_ports.contains(&6379));
        assert!(config.scrape_type.is_none());
        assert_eq!(child, vec!["app"]);
    }

    // service_name_from_child_binary test removed: env::remove_var is unsafe in Rust 2024 edition
    // and unsafe_code = "forbid". The fallback logic is exercised when PLATFORM_SERVICE_NAME is unset.

    #[test]
    fn defaults_transparent_mode() {
        let args: Vec<String> = vec!["--wrap".into(), "--".into(), "app".into()];
        let (config, _) = ProxyConfig::from_env_and_args(&args);
        assert!(!config.transparent);
        assert_eq!(config.mtls_mode, MtlsMode::Permissive);
        assert_eq!(config.inbound_port, 15006);
        // RFC1918 defaults
        assert_eq!(config.internal_cidrs.len(), 3);
        assert!(config.node_cidrs.is_empty());
        assert_eq!(config.bypass_port_range, (61000, 65000));
    }

    #[test]
    fn mtls_mode_parsing() {
        assert_eq!(MtlsMode::from_str_value("strict"), MtlsMode::Strict);
        assert_eq!(MtlsMode::from_str_value("STRICT"), MtlsMode::Strict);
        assert_eq!(MtlsMode::from_str_value("permissive"), MtlsMode::Permissive);
        assert_eq!(MtlsMode::from_str_value(""), MtlsMode::Permissive);
        assert_eq!(MtlsMode::from_str_value("other"), MtlsMode::Permissive);
    }
}
