//! HTTP connection pool for forwarding requests to backend pods.
//!
//! Maintains keep-alive connections per backend endpoint. For PR 7, plain HTTP
//! only (mTLS origination to backends added in PR 8).

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Connection pool for forwarding HTTP requests to backend endpoints.
///
/// Currently uses simple per-request TCP connections.
/// Future PRs will add persistent keep-alive connections and mTLS origination.
#[derive(Debug)]
pub struct ConnectionPool {
    /// Connect timeout.
    connect_timeout: std::time::Duration,
    /// Read timeout for backend responses.
    read_timeout: std::time::Duration,
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_secs(5),
            read_timeout: std::time::Duration::from_secs(30),
        }
    }
}

impl ConnectionPool {
    /// Create a new connection pool with default timeouts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forward an HTTP request to a backend endpoint and return the response.
    pub async fn forward(&self, backend: &SocketAddr, request: &[u8]) -> anyhow::Result<Vec<u8>> {
        // Connect to backend
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
        assert_eq!(pool.connect_timeout, std::time::Duration::from_secs(5));
        assert_eq!(pool.read_timeout, std::time::Duration::from_secs(30));
    }
}
