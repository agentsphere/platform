//! mTLS outbound proxy: apps connect here, proxy originates mTLS to upstream.

use std::net::SocketAddr;
use std::time::Instant;

use chrono::Utc;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use super::tls::{self, SharedCerts};
use super::traces::{self, SpanRecord};

/// Parameters for the outbound proxy.
pub struct OutboundParams {
    pub listen_port: u16,
    pub service_name: String,
    pub certs: SharedCerts,
    pub span_tx: mpsc::Sender<SpanRecord>,
}

/// Run the outbound mTLS proxy.
///
/// Listens on `localhost:PROXY_OUTBOUND_PORT` (default 15001).
/// Apps connect here instead of directly to upstream services.
/// The proxy originates mTLS to the destination's proxy port (8443).
#[tracing::instrument(skip_all, fields(port = params.listen_port))]
pub async fn run_outbound_proxy(params: OutboundParams, mut shutdown: watch::Receiver<()>) {
    let addr = SocketAddr::from(([127, 0, 0, 1], params.listen_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind outbound proxy");
            return;
        }
    };
    tracing::info!(port = params.listen_port, "outbound mTLS proxy started");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let certs = params.certs.clone();
                        let span_tx = params.span_tx.clone();
                        let service = params.service_name.clone();

                        tokio::spawn(Box::pin(async move {
                            if let Err(e) = handle_outbound_connection(
                                stream, certs, span_tx, &service,
                            ).await {
                                tracing::debug!(error = %e, "outbound connection error");
                            }
                        }));
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "outbound accept error");
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::debug!("outbound proxy exiting");
}

/// Handle a single outbound proxy connection.
async fn handle_outbound_connection(
    mut app_stream: TcpStream,
    certs: SharedCerts,
    span_tx: mpsc::Sender<SpanRecord>,
    service: &str,
) -> anyhow::Result<()> {
    // Read the HTTP request from the app
    let mut request_buf = vec![0u8; 16384];
    let n = app_stream.read(&mut request_buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request_bytes = &request_buf[..n];

    // Parse to extract Host header for routing
    let request_str = String::from_utf8_lossy(request_bytes);
    let (method, path, host, traceparent) = parse_outbound_request(&request_str);

    if host.is_empty() {
        let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 14\r\n\r\nMissing Host\r\n";
        app_stream.write_all(response).await?;
        return Ok(());
    }

    let start_time = Instant::now();
    let started_at = Utc::now();

    let (trace_id, parent_span_id) = traceparent
        .as_deref()
        .and_then(|tp| traces::parse_traceparent(tp).map(|(tid, psid, _)| (tid, Some(psid))))
        .unwrap_or_else(|| (traces::new_trace_id(), None));
    let span_id = traces::new_span_id();

    // Resolve destination: connect to host:8443 (upstream proxy's mTLS port)
    let dest_host = host.split(':').next().unwrap_or(&host);
    let dest_addr = format!("{dest_host}:8443");

    // Establish mTLS connection to upstream
    let current_certs = certs.load();
    let connector = tls::build_tls_connector(&current_certs)?;

    let upstream_tcp = TcpStream::connect(&dest_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to upstream {dest_addr}: {e}"))?;

    let server_name = ServerName::try_from(dest_host.to_string())
        .map_err(|e| anyhow::anyhow!("invalid server name '{dest_host}': {e}"))?;

    let mut tls_stream = connector
        .connect(server_name, upstream_tcp)
        .await
        .map_err(|e| anyhow::anyhow!("mTLS handshake to {dest_addr} failed: {e}"))?;

    // Inject traceparent and forward request
    let tp_header = traces::build_traceparent(&trace_id, &span_id);
    let forwarded = inject_outbound_traceparent(request_bytes, &tp_header);
    tls_stream.write_all(&forwarded).await?;

    // Read response from upstream
    let mut response_buf = Vec::with_capacity(16384);
    let mut temp = vec![0u8; 8192];
    loop {
        let nr = tls_stream.read(&mut temp).await?;
        if nr == 0 {
            break;
        }
        response_buf.extend_from_slice(&temp[..nr]);
    }

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start_time.elapsed().as_millis() as u64;
    let status_code = parse_response_status(&response_buf);

    if response_buf.is_empty() {
        app_stream
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await?;
    } else {
        app_stream.write_all(&response_buf).await?;
    }

    // Build and send CLIENT span
    let span_name = format!("{method} {path}");
    let span = traces::build_client_span(
        &trace_id,
        &span_id,
        parent_span_id.as_deref(),
        &span_name,
        service,
        started_at,
        i32::try_from(duration_ms).unwrap_or(i32::MAX),
        status_code,
    );
    let _ = span_tx.try_send(span);

    Ok(())
}

/// Parse outbound request for method, path, Host header, and traceparent.
fn parse_outbound_request(request: &str) -> (String, String, String, Option<String>) {
    let mut method = String::new();
    let mut path = String::new();
    let mut host = String::new();
    let mut traceparent = None;

    for (i, line) in request.lines().enumerate() {
        if i == 0 {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                method = parts[0].to_string();
                path = parts[1].to_string();
            }
        } else if line.is_empty() {
            break;
        } else {
            let lower = line.to_lowercase();
            if lower.starts_with("host:") {
                host = line
                    .split_once(':')
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default();
            } else if lower.starts_with("traceparent:") {
                traceparent = line.split_once(':').map(|(_, v)| v.trim().to_string());
            }
        }
    }

    (method, path, host, traceparent)
}

/// Parse HTTP status code from response.
fn parse_response_status(response: &[u8]) -> u16 {
    let s = String::from_utf8_lossy(response);
    s.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(502)
}

/// Inject traceparent header into outbound request.
fn inject_outbound_traceparent(request: &[u8], traceparent: &str) -> Vec<u8> {
    let req_str = String::from_utf8_lossy(request);
    let mut result = String::with_capacity(req_str.len() + 80);
    let mut found = false;
    let mut in_body = false;

    for line in req_str.split("\r\n") {
        if in_body {
            result.push_str(line);
            result.push_str("\r\n");
            continue;
        }
        if line.is_empty() {
            if !found {
                result.push_str("traceparent: ");
                result.push_str(traceparent);
                result.push_str("\r\n");
            }
            result.push_str("\r\n");
            in_body = true;
            continue;
        }
        if line.to_lowercase().starts_with("traceparent:") {
            result.push_str("traceparent: ");
            result.push_str(traceparent);
            result.push_str("\r\n");
            found = true;
        } else {
            result.push_str(line);
            result.push_str("\r\n");
        }
    }

    result.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outbound_request_basic() {
        let req = "GET /api/data HTTP/1.1\r\nHost: backend.default.svc:8080\r\ntraceparent: 00-aaa-bbb-01\r\n\r\n";
        let (method, path, host, tp) = parse_outbound_request(req);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/data");
        assert_eq!(host, "backend.default.svc:8080");
        assert_eq!(tp, Some("00-aaa-bbb-01".into()));
    }

    #[test]
    fn parse_outbound_request_no_host() {
        let req = "GET / HTTP/1.1\r\n\r\n";
        let (_, _, host, _) = parse_outbound_request(req);
        assert!(host.is_empty());
    }

    #[test]
    fn parse_response_status_basic() {
        assert_eq!(parse_response_status(b"HTTP/1.1 200 OK\r\n"), 200);
        assert_eq!(
            parse_response_status(b"HTTP/1.1 503 Service Unavailable\r\n"),
            503
        );
        assert_eq!(parse_response_status(b""), 502);
    }

    #[test]
    fn inject_outbound_traceparent_new() {
        let req = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";
        let result = inject_outbound_traceparent(req, "00-tid-sid-01");
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("traceparent: 00-tid-sid-01"));
    }

    #[test]
    fn inject_outbound_traceparent_replace() {
        let req = b"GET / HTTP/1.1\r\ntraceparent: old\r\nHost: test\r\n\r\n";
        let result = inject_outbound_traceparent(req, "00-new-01");
        let s = String::from_utf8(result).unwrap();
        assert!(s.contains("traceparent: 00-new-01"));
        assert!(!s.contains("old"));
    }
}
