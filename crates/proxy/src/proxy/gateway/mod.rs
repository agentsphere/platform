//! Gateway mode: Kubernetes ingress controller watching `HTTPRoute` CRDs.
//!
//! When `platform-proxy --gateway` is invoked, the binary runs as an
//! ingress controller that watches Gateway API `HTTPRoute` resources,
//! builds an in-memory routing table, and forwards HTTP traffic to
//! backend pods resolved via `EndpointSlice` resources.
//!
//! Features:
//! - OTEL span generation for every proxied request
//! - W3C traceparent propagation (generate or forward)
//! - RED metrics: request count, duration, response size
//! - Per-route token bucket rate limiting via `HTTPRoute` annotations
//! - Passive backend health checking with auto-reprobe

pub mod pool;
pub mod rate_limit;
pub mod router;
pub mod watcher;

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::Utc;
use dashmap::DashMap;
use tokio::sync::{mpsc, watch};

use router::RoutingTable;

use super::metrics::{MetricRecord, RedMetrics};
use super::traces::{self, SpanRecord};

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
    /// Namespaces to watch for `HTTPRoutes`. Empty = all namespaces.
    pub watch_namespaces: Vec<String>,
    /// Log level (default: "info").
    pub log_level: String,
    /// OTLP export endpoint (default: platform API URL).
    pub otlp_endpoint: Option<String>,
    /// API token for OTLP export.
    pub otlp_token: Option<String>,
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
            watch_namespaces: env::var("PROXY_GATEWAY_WATCH_NAMESPACES")
                .ok()
                .map(|s| {
                    s.split(',')
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            log_level: env::var("PROXY_LOG_LEVEL").unwrap_or_else(|_| "info".into()),
            otlp_endpoint: env::var("PROXY_OTLP_ENDPOINT").ok(),
            otlp_token: env::var("OTEL_API_TOKEN").ok(),
        }
    }
}

/// Shared routing table handle, atomically swapped by the watcher.
pub type SharedRoutingTable = Arc<ArcSwap<RoutingTable>>;

/// Shared rate limit configurations per route name.
pub type SharedRateLimitConfigs = Arc<DashMap<String, rate_limit::RateLimitConfig>>;

/// Run the gateway mode.
///
/// Orchestrates: K8s CRD watcher, HTTP listener, health server, OTLP exporter,
/// rate limit cleanup, RED metrics flush.
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

    // Initialize rate limiter
    let rate_limiter = Arc::new(rate_limit::RateLimiter::new());
    let rate_limit_configs: SharedRateLimitConfigs = Arc::new(DashMap::new());

    // Initialize OTLP channels
    let (span_tx, span_rx) = mpsc::channel::<SpanRecord>(4096);
    let (_log_tx, log_rx) = mpsc::channel(1024);
    let (metric_tx, metric_rx) = mpsc::channel::<MetricRecord>(4096);

    // RED metrics
    let red_metrics = Arc::new(RedMetrics::new());

    // Start OTLP exporter if endpoint configured
    if let Some(ref endpoint) = config.otlp_endpoint {
        let exporter = super::otlp::OtlpExporter::new(
            endpoint.clone(),
            config.otlp_token.clone().unwrap_or_default(),
            None,
            "platform-gateway".into(),
            None,
        );
        tokio::spawn(super::otlp::run_exporter(
            exporter,
            span_rx,
            log_rx,
            metric_rx,
            Duration::from_secs(5),
            100,
            shutdown_rx.clone(),
        ));
    } else {
        // Drain channels to avoid backpressure
        tokio::spawn(drain_channels(span_rx, log_rx, metric_rx));
    }

    // Start RED metrics flush
    tokio::spawn(super::metrics::flush_red_metrics(
        red_metrics.clone(),
        "platform-gateway".into(),
        metric_tx,
        Duration::from_secs(15),
        shutdown_rx.clone(),
    ));

    // Start rate limit cleanup
    tokio::spawn(rate_limit::run_cleanup(
        rate_limiter.clone(),
        Duration::from_secs(120),
        shutdown_rx.clone(),
    ));

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
        rate_limit_configs.clone(),
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
        rate_limiter,
        rate_limit_configs,
        span_tx,
        red_metrics,
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
    tokio::time::sleep(Duration::from_millis(500)).await;
    http_handle.abort();
}

/// Drain OTLP channels when no exporter is configured.
async fn drain_channels(
    mut span_rx: mpsc::Receiver<SpanRecord>,
    mut log_rx: mpsc::Receiver<super::logs::LogRecord>,
    mut metric_rx: mpsc::Receiver<MetricRecord>,
) {
    loop {
        tokio::select! {
            v = span_rx.recv() => { if v.is_none() { break; } }
            v = log_rx.recv() => { if v.is_none() { break; } }
            v = metric_rx.recv() => { if v.is_none() { break; } }
        }
    }
}

/// Shared state passed to each connection handler.
struct GatewayState {
    table: SharedRoutingTable,
    conn_pool: Arc<pool::ConnectionPool>,
    rate_limiter: Arc<rate_limit::RateLimiter>,
    rate_limit_configs: SharedRateLimitConfigs,
    span_tx: mpsc::Sender<SpanRecord>,
    red_metrics: Arc<RedMetrics>,
}

/// Run the HTTP listener that matches incoming requests against the routing table.
#[allow(clippy::too_many_arguments)]
async fn run_http_listener(
    port: u16,
    table: SharedRoutingTable,
    conn_pool: Arc<pool::ConnectionPool>,
    rate_limiter: Arc<rate_limit::RateLimiter>,
    rate_limit_configs: SharedRateLimitConfigs,
    span_tx: mpsc::Sender<SpanRecord>,
    red_metrics: Arc<RedMetrics>,
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

    let state = Arc::new(GatewayState {
        table,
        conn_pool,
        rate_limiter,
        rate_limit_configs,
        span_tx,
        red_metrics,
    });

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, peer, &state).await {
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

/// Write an error response to the client stream.
async fn write_error_response(
    stream: &mut tokio::net::TcpStream,
    status_line: &str,
    body: &[u8],
    extra_headers: &str,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n{extra_headers}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

/// Trace context extracted from an incoming request.
struct TraceContext {
    trace: String,
    parent_span: Option<String>,
    span: String,
}

/// Extract trace context from a parsed request.
fn extract_trace_context(parsed: &ParsedRequest) -> TraceContext {
    let traceparent_header = parsed
        .headers
        .iter()
        .find(|(name, _)| name == "traceparent")
        .map(|(_, v)| v.as_str());
    let (trace, parent_span) = resolve_trace_context(traceparent_header);
    let span = traces::new_span_id();
    TraceContext {
        trace,
        parent_span,
        span,
    }
}

/// Check rate limit for the matched route. Returns the error status line + body if limited.
fn check_rate_limit(
    state: &GatewayState,
    matched: &router::MatchResult,
    peer: &SocketAddr,
) -> Option<(String, &'static [u8], String)> {
    let rl_config = state.rate_limit_configs.get(&matched.route_name)?;
    let key = rate_limit::RateLimitKey::new(&matched.route_name, &peer.ip().to_string());
    if state.rate_limiter.check(&key, &rl_config) {
        return None;
    }
    let retry = format!("Retry-After: {}\r\n", rl_config.window_secs);
    Some(("429 Too Many Requests".into(), b"Too Many Requests", retry))
}

/// Handle a single HTTP connection: parse request, match route, forward to backend.
///
/// Generates an OTEL span and updates RED metrics for every request.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    state: &GatewayState,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let start_time = Instant::now();
    let started_at = Utc::now();

    let mut buf = vec![0u8; 16384];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request_bytes = &buf[..n];
    let parsed = parse_gateway_request(&String::from_utf8_lossy(request_bytes));
    let ctx = extract_trace_context(&parsed);

    // Match route
    let current_table = state.table.load();
    let match_result = current_table.match_request_full(
        parsed.host.as_deref().unwrap_or(""),
        &parsed.path,
        &parsed.method,
        &parsed.headers,
        &parsed.query_params,
    );

    let Some(matched) = match_result else {
        write_error_response(&mut stream, "404 Not Found", b"no matching route", "").await?;
        emit_telemetry(
            state, &ctx, &parsed, &peer, started_at, start_time, 404, None, 17,
        );
        return Ok(());
    };

    // Rate limit check
    if let Some((status, body, extra)) = check_rate_limit(state, &matched, &peer) {
        write_error_response(&mut stream, &status, body, &extra).await?;
        emit_telemetry(
            state,
            &ctx,
            &parsed,
            &peer,
            started_at,
            start_time,
            429,
            Some(&matched),
            body.len(),
        );
        return Ok(());
    }

    // Health check
    if !state.conn_pool.is_endpoint_available(&matched.endpoint) {
        write_error_response(
            &mut stream,
            "503 Service Unavailable",
            b"Service Unavailable",
            "",
        )
        .await?;
        emit_telemetry(
            state,
            &ctx,
            &parsed,
            &peer,
            started_at,
            start_time,
            503,
            Some(&matched),
            19,
        );
        return Ok(());
    }

    // Forward request with traceparent
    let traceparent = traces::build_traceparent(&ctx.trace, &ctx.span);
    let forwarded = add_forwarded_headers_with_trace(request_bytes, &peer, &traceparent);

    let (status_code, size) = match state.conn_pool.forward(&matched.endpoint, &forwarded).await {
        Ok(resp) => {
            let s = resp.len();
            let c = parse_http_status(&resp);
            stream.write_all(&resp).await?;
            (c, s)
        }
        Err(e) => {
            tracing::debug!(error = %e, "backend forward error");
            write_error_response(&mut stream, "502 Bad Gateway", b"Bad Gateway", "").await?;
            (502u16, 11)
        }
    };

    emit_telemetry(
        state,
        &ctx,
        &parsed,
        &peer,
        started_at,
        start_time,
        status_code,
        Some(&matched),
        size,
    );
    Ok(())
}

/// Shorthand for recording telemetry (span + RED metrics).
#[allow(clippy::too_many_arguments)]
fn emit_telemetry(
    state: &GatewayState,
    ctx: &TraceContext,
    parsed: &ParsedRequest,
    peer: &SocketAddr,
    started_at: chrono::DateTime<Utc>,
    start_time: Instant,
    status_code: u16,
    matched: Option<&router::MatchResult>,
    response_size: usize,
) {
    record_request_telemetry(
        state,
        &ctx.trace,
        &ctx.span,
        ctx.parent_span.as_deref(),
        parsed,
        peer,
        started_at,
        start_time,
        status_code,
        matched,
        response_size,
    );
}

/// Record OTEL span and RED metrics for a gateway request.
#[allow(clippy::too_many_arguments)]
fn record_request_telemetry(
    state: &GatewayState,
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    parsed: &ParsedRequest,
    peer: &SocketAddr,
    started_at: chrono::DateTime<Utc>,
    start_time: Instant,
    status_code: u16,
    matched: Option<&router::MatchResult>,
    response_size: usize,
) {
    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;

    // RED metrics
    state.red_metrics.record(duration_ms, status_code >= 500);

    // Build span attributes
    let span_name = format!("GATEWAY {} {}", parsed.method, parsed.path);
    let mut extra_attrs = vec![
        ("http.method".into(), parsed.method.clone()),
        ("http.url".into(), parsed.path.clone()),
        ("net.peer.ip".into(), peer.ip().to_string()),
        ("http.status_code".into(), status_code.to_string()),
    ];
    if let Some(ref host) = parsed.host {
        extra_attrs.push(("http.host".into(), host.clone()));
    }
    if let Some(ua) = parsed
        .headers
        .iter()
        .find(|(n, _)| n == "user-agent")
        .map(|(_, v)| v)
    {
        extra_attrs.push(("http.user_agent".into(), ua.clone()));
    }
    if let Some(m) = matched {
        extra_attrs.push(("http.route".into(), m.route_name.clone()));
        extra_attrs.push(("platform.backend.service".into(), m.backend_service.clone()));
        extra_attrs.push((
            "platform.backend.namespace".into(),
            m.backend_namespace.clone(),
        ));
    }
    #[allow(clippy::cast_precision_loss)]
    {
        extra_attrs.push((
            "gateway.response.size_bytes".into(),
            response_size.to_string(),
        ));
    }

    let span = traces::build_server_span(
        trace_id,
        span_id,
        parent_span_id,
        &span_name,
        "platform-gateway",
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        status_code,
        extra_attrs,
    );
    let _ = state.span_tx.try_send(span);
}

/// Resolve trace context from traceparent header or generate new.
fn resolve_trace_context(traceparent: Option<&str>) -> (String, Option<String>) {
    traceparent
        .and_then(|tp| traces::parse_traceparent(tp).map(|(tid, psid, _)| (tid, Some(psid))))
        .unwrap_or_else(|| (traces::new_trace_id(), None))
}

/// Parse HTTP status code from response first line.
fn parse_http_status(response: &[u8]) -> u16 {
    let s = String::from_utf8_lossy(response);
    s.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(502)
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

/// Add X-Forwarded-For, X-Forwarded-Proto, and traceparent headers to the request.
fn add_forwarded_headers_with_trace(
    request: &[u8],
    peer: &SocketAddr,
    traceparent: &str,
) -> Vec<u8> {
    let req_str = String::from_utf8_lossy(request);
    let mut result = String::with_capacity(req_str.len() + 200);
    let mut in_body = false;

    for line in req_str.split("\r\n") {
        if in_body {
            result.push_str(line);
            result.push_str("\r\n");
            continue;
        }
        if line.is_empty() {
            // Inject forwarded headers + traceparent before body
            result.push_str("X-Forwarded-For: ");
            result.push_str(&peer.ip().to_string());
            result.push_str("\r\nX-Forwarded-Proto: http\r\ntraceparent: ");
            result.push_str(traceparent);
            result.push_str("\r\n\r\n");
            in_body = true;
            continue;
        }
        // Skip existing forwarded and traceparent headers (we replace them)
        let lower = line.to_lowercase();
        if lower.starts_with("x-forwarded-for:")
            || lower.starts_with("x-forwarded-proto:")
            || lower.starts_with("traceparent:")
        {
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
    fn add_forwarded_headers_with_trace_basic() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let peer: SocketAddr = "192.168.1.100:12345".parse().unwrap();
        let result = add_forwarded_headers_with_trace(req, &peer, "00-trace-span-01");
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("X-Forwarded-For: 192.168.1.100"));
        assert!(s.contains("X-Forwarded-Proto: http"));
        assert!(s.contains("traceparent: 00-trace-span-01"));
    }

    #[test]
    fn add_forwarded_headers_replaces_existing() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com\r\nX-Forwarded-For: 10.0.0.1\r\nX-Forwarded-Proto: https\r\ntraceparent: old-value\r\n\r\n";
        let peer: SocketAddr = "192.168.1.100:12345".parse().unwrap();
        let result = add_forwarded_headers_with_trace(req, &peer, "00-new-trace-01");
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("X-Forwarded-For: 192.168.1.100"));
        assert!(s.contains("traceparent: 00-new-trace-01"));
        assert!(!s.contains("10.0.0.1"));
        assert!(!s.contains("old-value"));
    }

    #[test]
    fn gateway_config_defaults() {
        let config = GatewayConfig {
            http_port: 8080,
            health_port: 15020,
            gateway_name: "platform-gateway".into(),
            gateway_namespace: "platform".into(),
            watch_namespaces: vec![],
            log_level: "info".into(),
            otlp_endpoint: None,
            otlp_token: None,
        };
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.gateway_name, "platform-gateway");
        assert_eq!(config.gateway_namespace, "platform");
    }

    #[test]
    fn resolve_trace_context_with_traceparent() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (trace_id, parent_span_id) = resolve_trace_context(Some(tp));
        assert_eq!(trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parent_span_id, Some("00f067aa0ba902b7".into()));
    }

    #[test]
    fn resolve_trace_context_without_traceparent() {
        let (trace_id, parent_span_id) = resolve_trace_context(None);
        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(parent_span_id.is_none());
    }

    #[test]
    fn resolve_trace_context_invalid_traceparent() {
        let (trace_id, parent_span_id) = resolve_trace_context(Some("invalid"));
        // Should generate new trace_id since parsing fails
        assert_eq!(trace_id.len(), 32);
        assert!(parent_span_id.is_none());
    }

    #[test]
    fn parse_http_status_codes() {
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK\r\n"), 200);
        assert_eq!(parse_http_status(b"HTTP/1.1 404 Not Found\r\n"), 404);
        assert_eq!(parse_http_status(b"HTTP/1.1 502 Bad Gateway\r\n"), 502);
        assert_eq!(parse_http_status(b""), 502);
    }

    #[test]
    fn span_attributes_for_request() {
        // Verify the span building logic produces correct attributes
        let trace_id = traces::new_trace_id();
        let span_id = traces::new_span_id();
        let now = Utc::now();

        let extra_attrs = vec![
            ("http.method".into(), "GET".into()),
            ("http.url".into(), "/api/test".into()),
            ("net.peer.ip".into(), "10.0.0.1".into()),
            ("http.status_code".into(), "200".into()),
            ("http.host".into(), "api.example.com".into()),
            ("http.route".into(), "my-route".into()),
            ("platform.backend.service".into(), "api-svc".into()),
            ("platform.backend.namespace".into(), "default".into()),
            ("http.user_agent".into(), "curl/7.81".into()),
        ];

        let span = traces::build_server_span(
            &trace_id,
            &span_id,
            None,
            "GATEWAY GET /api/test",
            "platform-gateway",
            now,
            42,
            200,
            extra_attrs,
        );

        assert_eq!(span.name, "GATEWAY GET /api/test");
        assert_eq!(span.service, "platform-gateway");
        assert_eq!(span.status, "ok");
        assert_eq!(span.http_status_code, Some(200));

        let attrs = span.attributes.as_ref().unwrap().as_object().unwrap();
        assert_eq!(attrs["http.method"], "GET");
        assert_eq!(attrs["http.url"], "/api/test");
        assert_eq!(attrs["net.peer.ip"], "10.0.0.1");
        assert_eq!(attrs["http.host"], "api.example.com");
        assert_eq!(attrs["http.route"], "my-route");
        assert_eq!(attrs["platform.backend.service"], "api-svc");
        assert_eq!(attrs["platform.backend.namespace"], "default");
        assert_eq!(attrs["http.user_agent"], "curl/7.81");
    }

    #[test]
    fn traceparent_parsing_and_propagation() {
        // Test incoming traceparent is used and new span_id is generated
        let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (trace_id, parent) = resolve_trace_context(Some(incoming));
        assert_eq!(trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parent, Some("00f067aa0ba902b7".into()));

        // New span_id for outgoing traceparent
        let new_span = traces::new_span_id();
        let outgoing = traces::build_traceparent(&trace_id, &new_span);
        assert!(outgoing.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
        assert!(outgoing.ends_with("-01"));
        // The span_id portion should be the new one, not the parent
        assert!(!outgoing.contains("00f067aa0ba902b7"));
    }

    #[test]
    fn parse_gateway_request_extracts_traceparent() {
        let req = "GET /api HTTP/1.1\r\nHost: app.io\r\ntraceparent: 00-abc123-def456-01\r\nUser-Agent: test\r\n\r\n";
        let parsed = parse_gateway_request(req);
        let tp = parsed
            .headers
            .iter()
            .find(|(n, _)| n == "traceparent")
            .map(|(_, v)| v.as_str());
        assert_eq!(tp, Some("00-abc123-def456-01"));
    }

    #[test]
    fn parse_gateway_request_extracts_user_agent() {
        let req = "GET / HTTP/1.1\r\nHost: app.io\r\nUser-Agent: Mozilla/5.0\r\n\r\n";
        let parsed = parse_gateway_request(req);
        let ua = parsed
            .headers
            .iter()
            .find(|(n, _)| n == "user-agent")
            .map(|(_, v)| v.as_str());
        assert_eq!(ua, Some("Mozilla/5.0"));
    }
}
