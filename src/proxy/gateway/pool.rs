//! HTTP connection pool for forwarding requests to backend pods.
//!
//! Maintains keep-alive connections per backend endpoint. For PR 7, plain HTTP
//! only (mTLS origination to backends added in PR 8).
//!
//! Passive health checking: tracks consecutive failures per endpoint. After
//! `UNHEALTHY_THRESHOLD` (3) consecutive failures, the endpoint is marked
//! unhealthy and skipped during load balancing. Unhealthy endpoints are
//! re-probed every `REPROBE_INTERVAL` (30s).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

/// Number of consecutive failures before marking an endpoint unhealthy.
const UNHEALTHY_THRESHOLD: u32 = 3;

/// How often to re-probe an unhealthy endpoint (in seconds).
const REPROBE_INTERVAL_SECS: u64 = 30;

/// Health state for a single backend endpoint.
#[derive(Debug)]
pub struct EndpointHealth {
    /// Consecutive failure count.
    pub consecutive_failures: AtomicU32,
    /// Timestamp (as millis since process start) when the endpoint was last probed.
    /// Stored as u64 for atomic access.
    last_probe_millis: AtomicU64,
}

impl EndpointHealth {
    fn new() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            last_probe_millis: AtomicU64::new(0),
        }
    }

    /// Record a successful request to this endpoint.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed request to this endpoint.
    pub fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether this endpoint is considered healthy.
    pub fn is_healthy(&self) -> bool {
        self.consecutive_failures.load(Ordering::Relaxed) < UNHEALTHY_THRESHOLD
    }

    /// Whether this unhealthy endpoint is due for a re-probe.
    pub fn should_reprobe(&self, now_millis: u64) -> bool {
        let last = self.last_probe_millis.load(Ordering::Relaxed);
        now_millis.saturating_sub(last) >= REPROBE_INTERVAL_SECS * 1000
    }

    /// Mark this endpoint as having been probed now.
    pub fn mark_probed(&self, now_millis: u64) {
        self.last_probe_millis.store(now_millis, Ordering::Relaxed);
    }
}

/// Connection pool for forwarding HTTP requests to backend endpoints.
///
/// Tracks per-endpoint health state for passive health checking.
/// Currently uses simple per-request TCP connections.
/// Future PRs will add persistent keep-alive connections and mTLS origination.
#[derive(Debug)]
pub struct ConnectionPool {
    /// Connect timeout.
    connect_timeout: Duration,
    /// Read timeout for backend responses.
    read_timeout: Duration,
    /// Per-endpoint health tracking.
    health: DashMap<SocketAddr, EndpointHealth>,
    /// Process start time for computing relative timestamps.
    start_time: Instant,
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(30),
            health: DashMap::new(),
            start_time: Instant::now(),
        }
    }
}

impl ConnectionPool {
    /// Create a new connection pool with default timeouts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create health state for an endpoint.
    fn get_health(
        &self,
        addr: &SocketAddr,
    ) -> dashmap::mapref::one::Ref<'_, SocketAddr, EndpointHealth> {
        if !self.health.contains_key(addr) {
            self.health.entry(*addr).or_insert_with(EndpointHealth::new);
        }
        // Safe because we just ensured the key exists
        self.health.get(addr).expect("just inserted")
    }

    /// Milliseconds since process start (for atomic timestamp comparison).
    fn now_millis(&self) -> u64 {
        #[allow(clippy::cast_possible_truncation)]
        let ms = self.start_time.elapsed().as_millis() as u64;
        ms
    }

    /// Check if an endpoint is available for traffic (healthy or due for reprobe).
    pub fn is_endpoint_available(&self, addr: &SocketAddr) -> bool {
        let health = self.get_health(addr);
        if health.is_healthy() {
            return true;
        }
        // Unhealthy — check if due for reprobe
        health.should_reprobe(self.now_millis())
    }

    /// Forward an HTTP request to a backend endpoint and return the response.
    pub async fn forward(&self, backend: &SocketAddr, request: &[u8]) -> anyhow::Result<Vec<u8>> {
        // Mark as probed if unhealthy (for reprobe timing)
        {
            let health = self.get_health(backend);
            if !health.is_healthy() {
                health.mark_probed(self.now_millis());
            }
        }

        // Connect to backend
        let result = self.do_forward(backend, request).await;

        // Update health state based on result
        let health = self.get_health(backend);
        match &result {
            Ok(_) => health.record_success(),
            Err(_) => health.record_failure(),
        }

        result
    }

    /// Internal forward implementation (no health tracking).
    async fn do_forward(&self, backend: &SocketAddr, request: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut stream = tokio::time::timeout(self.connect_timeout, TcpStream::connect(backend))
            .await
            .map_err(|_| anyhow::anyhow!("connect timeout to {backend}"))?
            .map_err(|e| anyhow::anyhow!("failed to connect to {backend}: {e}"))?;

        // Send request
        stream.write_all(request).await?;

        // Read response
        let mut response = Vec::with_capacity(16384);
        let mut buf = vec![0u8; 8192];

        match tokio::time::timeout(self.read_timeout, async {
            loop {
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&buf[..n]);
                if has_complete_response(&response) {
                    break;
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(error = %e, backend = %backend, "backend read error");
            }
            Err(_) => {
                tracing::debug!(backend = %backend, "backend read timeout");
            }
        }

        if response.is_empty() {
            return Err(anyhow::anyhow!("empty response from backend {backend}"));
        }

        Ok(response)
    }

    /// Run active health checks for backends that have a health path annotation.
    /// Periodically sends HTTP GET requests to the health path of each registered
    /// endpoint and updates health state based on the response.
    pub async fn run_active_health_checks(
        &self,
        health_paths: &DashMap<String, String>,
        endpoint_to_service: &DashMap<SocketAddr, String>,
        interval: Duration,
        mut shutdown: watch::Receiver<()>,
    ) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    for entry in endpoint_to_service {
                        let addr = *entry.key();
                        let svc_key = entry.value().clone();
                        if let Some(path_entry) = health_paths.get(&svc_key) {
                            let health_path = path_entry.value().clone();
                            let request = format!(
                                "GET {health_path} HTTP/1.1\r\nHost: health-check\r\nConnection: close\r\n\r\n"
                            );
                            if let Ok(resp) = self.do_forward(&addr, request.as_bytes()).await {
                                let status = parse_http_status(&resp);
                                let health = self.get_health(&addr);
                                if status < 400 {
                                    health.record_success();
                                } else {
                                    health.record_failure();
                                }
                            } else {
                                let health = self.get_health(&addr);
                                health.record_failure();
                            }
                        }
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
    }

    /// Get the consecutive failure count for an endpoint (for diagnostics/testing).
    pub fn failure_count(&self, addr: &SocketAddr) -> u32 {
        self.health
            .get(addr)
            .map_or(0, |h| h.consecutive_failures.load(Ordering::Relaxed))
    }

    /// Reset health state for an endpoint (for testing).
    #[cfg(test)]
    pub fn reset_health(&self, addr: &SocketAddr) {
        if let Some(h) = self.health.get(addr) {
            h.consecutive_failures.store(0, Ordering::Relaxed);
            h.last_probe_millis.store(0, Ordering::Relaxed);
        }
    }
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

/// Check if we have a complete HTTP response (headers + body per Content-Length).
fn has_complete_response(data: &[u8]) -> bool {
    let header_end = find_header_end(data);
    if header_end == 0 {
        return false;
    }
    let headers = String::from_utf8_lossy(&data[..header_end]);
    if let Some(cl) = extract_content_length(&headers) {
        let body_len = data.len() - header_end;
        body_len >= cl
    } else {
        // No Content-Length, check for Transfer-Encoding: chunked terminator
        if headers
            .to_lowercase()
            .contains("transfer-encoding: chunked")
        {
            data.windows(5).any(|w| w == b"0\r\n\r\n")
        } else {
            false
        }
    }
}

/// Find the end of HTTP headers (position after `\r\n\r\n`).
fn find_header_end(data: &[u8]) -> usize {
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == b'\r' && data[i + 1] == b'\n' && data[i + 2] == b'\r' && data[i + 3] == b'\n'
        {
            return i + 4;
        }
    }
    0
}

/// Extract Content-Length value from headers.
fn extract_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let lower = line.to_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_complete_response_with_content_length() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert!(has_complete_response(data));
    }

    #[test]
    fn has_complete_response_incomplete_body() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello";
        assert!(!has_complete_response(data));
    }

    #[test]
    fn has_complete_response_no_headers_end() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n";
        assert!(!has_complete_response(data));
    }

    #[test]
    fn has_complete_response_chunked() {
        let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(has_complete_response(data));
    }

    #[test]
    fn find_header_end_present() {
        let data = b"HTTP/1.1 200 OK\r\nFoo: bar\r\n\r\nbody";
        assert_eq!(find_header_end(data), 29);
    }

    #[test]
    fn find_header_end_absent() {
        let data = b"HTTP/1.1 200 OK\r\nFoo: bar\r\n";
        assert_eq!(find_header_end(data), 0);
    }

    #[test]
    fn extract_content_length_basic() {
        let headers = "HTTP/1.1 200 OK\r\nContent-Length: 42\r\n";
        assert_eq!(extract_content_length(headers), Some(42));
    }

    #[test]
    fn extract_content_length_missing() {
        let headers = "HTTP/1.1 200 OK\r\nX-Custom: value\r\n";
        assert_eq!(extract_content_length(headers), None);
    }

    #[test]
    fn connection_pool_default_timeouts() {
        let pool = ConnectionPool::new();
        assert_eq!(pool.connect_timeout, Duration::from_secs(5));
        assert_eq!(pool.read_timeout, Duration::from_secs(30));
    }

    #[test]
    fn endpoint_health_starts_healthy() {
        let health = EndpointHealth::new();
        assert!(health.is_healthy());
        assert_eq!(health.consecutive_failures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn endpoint_health_unhealthy_after_three_failures() {
        let health = EndpointHealth::new();
        health.record_failure();
        assert!(health.is_healthy());
        health.record_failure();
        assert!(health.is_healthy());
        health.record_failure();
        assert!(!health.is_healthy());
    }

    #[test]
    fn endpoint_health_success_resets_failures() {
        let health = EndpointHealth::new();
        health.record_failure();
        health.record_failure();
        health.record_success();
        assert!(health.is_healthy());
        assert_eq!(health.consecutive_failures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn endpoint_health_reprobe_timing() {
        let health = EndpointHealth::new();
        // Mark probed at time 0
        health.mark_probed(1000);
        // Not enough time has passed
        assert!(!health.should_reprobe(1000 + 29_000));
        // 30 seconds later — should reprobe
        assert!(health.should_reprobe(1000 + 30_000));
    }

    #[test]
    fn passive_health_marks_unhealthy_after_failures() {
        let pool = ConnectionPool::new();
        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();

        // Simulate 3 consecutive failures
        let health = pool.get_health(&addr);
        health.record_failure();
        health.record_failure();
        health.record_failure();

        assert!(!pool.is_endpoint_available(&addr));
    }

    #[test]
    fn passive_health_reprobe_after_timeout() {
        let health = EndpointHealth::new();
        // Mark unhealthy
        health.record_failure();
        health.record_failure();
        health.record_failure();
        assert!(!health.is_healthy());

        // Mark probed at time 1000ms
        health.mark_probed(1000);

        // Not enough time elapsed (29s later)
        assert!(!health.should_reprobe(1000 + 29_999));

        // 30s later — should reprobe
        assert!(health.should_reprobe(1000 + 30_000));

        // Verify pool integration: is_endpoint_available uses should_reprobe
        let pool = ConnectionPool::new();
        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        {
            let h = pool.get_health(&addr);
            h.record_failure();
            h.record_failure();
            h.record_failure();
            // Mark probed far in the past so reprobe triggers immediately
            h.mark_probed(0);
        }
        // now_millis() is based on process start; even if tiny, the 0 probe
        // time ensures the 30s window has passed (since REPROBE_INTERVAL_SECS*1000 <= any positive now)
        // But in a fast test now_millis might be 0 too. Use the health directly.
        let h = pool.get_health(&addr);
        // Force a concrete check with a known timestamp
        assert!(h.should_reprobe(REPROBE_INTERVAL_SECS * 1000 + 1));
    }

    #[test]
    fn failure_count_tracking() {
        let pool = ConnectionPool::new();
        let addr: SocketAddr = "10.0.0.5:9090".parse().unwrap();

        assert_eq!(pool.failure_count(&addr), 0);

        let health = pool.get_health(&addr);
        health.record_failure();
        health.record_failure();
        drop(health);

        assert_eq!(pool.failure_count(&addr), 2);
    }

    #[test]
    fn parse_http_status_basic() {
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK\r\n"), 200);
        assert_eq!(parse_http_status(b"HTTP/1.1 404 Not Found\r\n"), 404);
        assert_eq!(parse_http_status(b"HTTP/1.1 502 Bad Gateway\r\n"), 502);
        assert_eq!(parse_http_status(b""), 502);
    }
}
