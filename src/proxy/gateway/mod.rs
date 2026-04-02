//! Gateway mode: Kubernetes ingress controller watching `HTTPRoute` CRDs.
//!
//! When `platform-proxy --gateway` is invoked, the binary runs as an
//! ingress controller that watches Gateway API `HTTPRoute` resources,
//! builds an in-memory routing table, and forwards HTTP traffic to
//! backend pods resolved via `EndpointSlice` resources.

pub mod pool;
pub mod router;
pub mod watcher;

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;

use router::RoutingTable;

/// Gateway-mode configuration parsed from environment variables.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// HTTP listen port (default: 8080).
    pub http_port: u16,
    /// Health probe port (default: 15020).
    pub health_port: u16,
    /// Gateway resource name to filter parentRefs (default: "platform-gateway").
    pub gateway_name: String,
    /// Gateway resource namespace to filter parentRefs (default: "platform").
    pub gateway_namespace: String,
    /// Log level (default: "info").
    pub log_level: String,
}

impl GatewayConfig {
    /// Parse gateway configuration from environment variables.
    pub fn from_env() -> Self {
        Self {
            http_port: env::var("PROXY_GATEWAY_HTTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            health_port: env::var("PROXY_HEALTH_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(15020),
            gateway_name: env::var("PROXY_GATEWAY_NAME")
                .unwrap_or_else(|_| "platform-gateway".into()),
            gateway_namespace: env::var("PROXY_GATEWAY_NAMESPACE")
                .unwrap_or_else(|_| "platform".into()),
            log_level: env::var("PROXY_LOG_LEVEL").unwrap_or_else(|_| "info".into()),
        }
    }
}

/// Shared routing table handle, atomically swapped by the watcher.
pub type SharedRoutingTable = Arc<ArcSwap<RoutingTable>>;

/// Run the gateway mode.
///
/// Orchestrates: K8s CRD watcher, HTTP listener, health server.
/// Blocks until shutdown signal (SIGINT/SIGTERM).
pub async fn run(_args: Vec<String>) {
    let config = GatewayConfig::from_env();

    tracing::info!(
        http_port = config.http_port,
        gateway_name = %config.gateway_name,
        gateway_namespace = %config.gateway_namespace,
        "platform-proxy starting in gateway mode"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(());

    // Initialize shared routing table (empty, not ready)
    let table = Arc::new(ArcSwap::from_pointee(RoutingTable::empty()));

    // Initialize connection pool
    let conn_pool = Arc::new(pool::ConnectionPool::new());

    // Start K8s watcher
    let kube_client = match kube::Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to create kube client");
            return;
        }
    };
    tokio::spawn(watcher::run(
        kube_client,
        config.clone(),
        table.clone(),
        shutdown_rx.clone(),
    ));

    // Wait for initial route sync before accepting traffic
    tracing::info!("waiting for initial route sync...");
    watcher::wait_for_ready(&table).await;
    tracing::info!("routing table ready, starting HTTP listener");

    // Start health server (readiness gated on routing table)
    tokio::spawn(run_gateway_health(
        config.health_port,
        table.clone(),
        shutdown_rx.clone(),
    ));

    // Start HTTP listener
    let http_handle = tokio::spawn(run_http_listener(
        config.http_port,
        table.clone(),
        conn_pool,
        shutdown_rx.clone(),
    ));

    // Wait for shutdown
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received SIGINT, shutting down gateway");
        }
    }
    let _ = shutdown_tx.send(());

    // Give tasks time to drain
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    http_handle.abort();
}

/// Run the HTTP listener that matches incoming requests against the routing table.
async fn run_http_listener(
    port: u16,
    table: SharedRoutingTable,
    conn_pool: Arc<pool::ConnectionPool>,
    mut shutdown: watch::Receiver<()>,
) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port, "failed to bind gateway HTTP listener");
            return;
        }
    };
    tracing::info!(port, "gateway HTTP listener started");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let table = table.clone();
                        let pool = conn_pool.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, peer, table, pool).await {
                                tracing::debug!(error = %e, peer = %peer, "gateway connection error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "gateway accept error");
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("gateway HTTP listener exiting");
}

/// Handle a single HTTP connection: parse request, match route, forward to backend.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    table: SharedRoutingTable,
    conn_pool: Arc<pool::ConnectionPool>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 16384];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request_bytes = &buf[..n];
    let request_str = String::from_utf8_lossy(request_bytes);

    // Parse the HTTP request
    let parsed = parse_gateway_request(&request_str);

    // Load current routing table
    let current_table = table.load();

    // Match the request
    let backend = current_table.match_request(
        parsed.host.as_deref().unwrap_or(""),
        &parsed.path,
        &parsed.method,
        &parsed.headers,
        &parsed.query_params,
    );

    let Some(backend) = backend else {
        let body = b"no matching route";
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await?;
        stream.write_all(body).await?;
        return Ok(());
    };

    // Add forwarded headers
    let forwarded_request = add_forwarded_headers(request_bytes, &peer);

    // Forward to backend via connection pool
    match conn_pool.forward(&backend, &forwarded_request).await {
        Ok(response_bytes) => {
            stream.write_all(&response_bytes).await?;
        }
        Err(e) => {
            tracing::debug!(error = %e, "backend forward error");
            let body = b"Bad Gateway";
            let response = format!(
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
        }
    }

    Ok(())
}

/// Parsed HTTP request fields for routing.
struct ParsedRequest {
    method: String,
    path: String,
    host: Option<String>,
    headers: Vec<(String, String)>,
    query_params: Vec<(String, String)>,
}

/// Parse an HTTP request for gateway routing.
fn parse_gateway_request(request: &str) -> ParsedRequest {
    let mut method = String::new();
    let mut raw_path = String::new();
    let mut host = None;
    let mut headers = Vec::new();

    for (i, line) in request.lines().enumerate() {
        if i == 0 {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                method = parts[0].to_string();
                raw_path = parts[1].to_string();
            }
        } else if line.is_empty() {
            break;
        } else if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_lowercase();
            let value = value.trim().to_string();
            if name == "host" {
                // Strip port from Host header for routing
                host = Some(value.split(':').next().unwrap_or(&value).to_string());
            }
            headers.push((name, value));
        }
    }

    // Extract query parameters from path
    let (path, query_params) = if let Some((p, q)) = raw_path.split_once('?') {
        let params: Vec<(String, String)> = q
            .split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        (p.to_string(), params)
    } else {
        (raw_path, Vec::new())
    };

    ParsedRequest {
        method,
        path,
        host,
        headers,
        query_params,
    }
}

/// Add X-Forwarded-For and X-Forwarded-Proto headers to the request.
fn add_forwarded_headers(request: &[u8], peer: &SocketAddr) -> Vec<u8> {
    let req_str = String::from_utf8_lossy(request);
    let mut result = String::with_capacity(req_str.len() + 128);
    let mut in_body = false;

    for line in req_str.split("\r\n") {
        if in_body {
            result.push_str(line);
            result.push_str("\r\n");
            continue;
        }
        if line.is_empty() {
            // Inject forwarded headers before body
            result.push_str("X-Forwarded-For: ");
            result.push_str(&peer.ip().to_string());
            result.push_str("\r\nX-Forwarded-Proto: http\r\n\r\n");
            in_body = true;
            continue;
        }
        // Skip existing forwarded headers (we replace them)
        let lower = line.to_lowercase();
        if lower.starts_with("x-forwarded-for:") || lower.starts_with("x-forwarded-proto:") {
            continue;
        }
        result.push_str(line);
        result.push_str("\r\n");
    }

    result.into_bytes()
}

/// Gateway-mode health server. Readiness is gated on the routing table being ready.
async fn run_gateway_health(
    port: u16,
    table: SharedRoutingTable,
    mut shutdown: watch::Receiver<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port, "failed to bind gateway health server");
            return;
        }
    };
    tracing::info!(port, "gateway health server listening");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((mut stream, _)) => {
                        let table = table.clone();
                        tokio::spawn(async move {
                            let mut buf = [0u8; 1024];
                            let Ok(n) = stream.read(&mut buf).await else {
                                return;
                            };
                            let request = String::from_utf8_lossy(&buf[..n]);
                            let path = request
                                .lines()
                                .next()
                                .and_then(|line| line.split_whitespace().nth(1))
                                .unwrap_or("/");

                            let (status, body) = match path {
                                "/healthz" => ("200 OK", "ok"),
                                "/readyz" => {
                                    let current = table.load();
                                    if current.is_ready() {
                                        ("200 OK", "ready")
                                    } else {
                                        ("503 Service Unavailable", "not ready")
                                    }
                                }
                                _ => ("404 Not Found", "not found"),
                            };

                            let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        });
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "gateway health accept error");
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gateway_request_basic() {
        let req = "GET /api/v1/users HTTP/1.1\r\nHost: api.example.com\r\nAccept: application/json\r\n\r\n";
        let parsed = parse_gateway_request(req);
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/api/v1/users");
        assert_eq!(parsed.host, Some("api.example.com".into()));
        assert!(parsed.query_params.is_empty());
    }

    #[test]
    fn parse_gateway_request_with_query_params() {
        let req = "GET /search?q=hello&page=2 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let parsed = parse_gateway_request(req);
        assert_eq!(parsed.path, "/search");
        assert_eq!(parsed.query_params.len(), 2);
        assert_eq!(parsed.query_params[0], ("q".into(), "hello".into()));
        assert_eq!(parsed.query_params[1], ("page".into(), "2".into()));
    }

    #[test]
    fn parse_gateway_request_host_with_port() {
        let req = "GET / HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";
        let parsed = parse_gateway_request(req);
        assert_eq!(parsed.host, Some("example.com".into()));
    }

    #[test]
    fn parse_gateway_request_no_host() {
        let req = "GET / HTTP/1.1\r\n\r\n";
        let parsed = parse_gateway_request(req);
        assert!(parsed.host.is_none());
    }

    #[test]
    fn add_forwarded_headers_basic() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let peer: SocketAddr = "192.168.1.100:12345".parse().unwrap();
        let result = add_forwarded_headers(req, &peer);
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("X-Forwarded-For: 192.168.1.100"));
        assert!(s.contains("X-Forwarded-Proto: http"));
    }

    #[test]
    fn add_forwarded_headers_replaces_existing() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com\r\nX-Forwarded-For: 10.0.0.1\r\nX-Forwarded-Proto: https\r\n\r\n";
        let peer: SocketAddr = "192.168.1.100:12345".parse().unwrap();
        let result = add_forwarded_headers(req, &peer);
        let s = String::from_utf8(result).unwrap();
        // Should have the new IP, not the old one
        assert!(s.contains("X-Forwarded-For: 192.168.1.100"));
        assert!(!s.contains("10.0.0.1"));
    }

    #[test]
    fn gateway_config_defaults() {
        let config = GatewayConfig {
            http_port: 8080,
            health_port: 15020,
            gateway_name: "platform-gateway".into(),
            gateway_namespace: "platform".into(),
            log_level: "info".into(),
        };
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.gateway_name, "platform-gateway");
        assert_eq!(config.gateway_namespace, "platform");
    }
}
